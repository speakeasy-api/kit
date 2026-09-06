//! Terminal input captured independently of the TUI's processing and rendering.

use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::Notify;

const MAX_QUEUED_EVENTS: usize = 1024;
const MAX_QUEUED_BYTES: usize = 256 * 1024;
const RECOVERY_QUIET: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct ReceivedEvent {
    pub event: Event,
    pub received_at: Instant,
}

#[derive(Debug)]
pub enum InputEvent {
    Event(ReceivedEvent),
    Overflow,
    RecoveryReady(bool),
    Resumed(Instant),
}

#[derive(Default)]
enum Recovery {
    #[default]
    Normal,
    DropUntilQuiet(Instant),
    AwaitEsc,
    AfterEsc {
        last_input: Instant,
        acknowledged_at: Instant,
    },
}

/// Bounded payload storage with constant-sized, out-of-band recovery metadata.
#[derive(Default)]
struct InputQueue {
    events: VecDeque<ReceivedEvent>,
    queued_bytes: usize,
    overflow: bool,
    // Coalesce readiness changes: activity must revoke even an unconsumed ready.
    readiness: Option<bool>,
    resumed: Option<Instant>,
    recovery: Recovery,
    error: Option<io::Error>,
    closed: bool,
}

impl InputQueue {
    fn payload_bytes(received: &ReceivedEvent) -> usize {
        match &received.event {
            Event::Paste(text) => text.capacity(),
            _ => 0,
        }
    }

    fn push(&mut self, received: ReceivedEvent) -> bool {
        if self.closed {
            return false;
        }
        match &mut self.recovery {
            Recovery::DropUntilQuiet(last_input) => {
                *last_input = received.received_at;
                return false;
            }
            Recovery::AwaitEsc => {
                self.recovery = if matches!(
                    &received.event,
                    Event::Key(key)
                        if key.code == KeyCode::Esc
                            && key.modifiers == KeyModifiers::NONE
                            && key.kind == KeyEventKind::Press
                ) {
                    Recovery::AfterEsc {
                        last_input: received.received_at,
                        acknowledged_at: received.received_at,
                    }
                } else {
                    Recovery::DropUntilQuiet(received.received_at)
                };
                self.readiness = Some(false);
                return true;
            }
            Recovery::AfterEsc { last_input, .. } => {
                *last_input = received.received_at;
                return false;
            }
            Recovery::Normal => {}
        }

        let bytes = Self::payload_bytes(&received);
        if self.events.len() >= MAX_QUEUED_EVENTS || bytes > MAX_QUEUED_BYTES - self.queued_bytes {
            self.events.clear();
            self.queued_bytes = 0;
            self.overflow = true;
            self.readiness = None;
            self.resumed = None;
            self.recovery = Recovery::DropUntilQuiet(received.received_at);
            return true;
        }
        self.queued_bytes += bytes;
        self.events.push_back(received);
        true
    }

    /// Only an unsuccessful terminal poll can establish quiet; event timestamps
    /// alone cannot prove that the terminal's input backlog has drained.
    fn observe_quiet(&mut self, now: Instant) -> bool {
        if self.closed {
            return false;
        }
        match self.recovery {
            Recovery::DropUntilQuiet(last_input)
                if now.saturating_duration_since(last_input) >= RECOVERY_QUIET =>
            {
                self.recovery = Recovery::AwaitEsc;
                self.readiness = Some(true);
                return true;
            }
            Recovery::AfterEsc {
                last_input,
                acknowledged_at,
            } if now.saturating_duration_since(last_input) >= RECOVERY_QUIET => {
                self.readiness = None;
                self.resumed = Some(acknowledged_at);
                self.recovery = Recovery::Normal;
                return true;
            }
            _ => {}
        }
        false
    }

    fn pop(&mut self) -> Option<io::Result<InputEvent>> {
        if self.overflow {
            self.overflow = false;
            return Some(Ok(InputEvent::Overflow));
        }
        if let Some(ready) = self.readiness.take() {
            return Some(Ok(InputEvent::RecoveryReady(ready)));
        }
        if let Some(acknowledged_at) = self.resumed.take() {
            return Some(Ok(InputEvent::Resumed(acknowledged_at)));
        }
        if let Some(received) = self.events.pop_front() {
            self.queued_bytes -= Self::payload_bytes(&received);
            return Some(Ok(InputEvent::Event(received)));
        }
        self.error.take().map(Err)
    }

    fn finish(&mut self, error: Option<io::Error>) {
        if !self.closed {
            self.error = error;
            self.closed = true;
        }
    }
}

/// Owns the terminal's sole event reader.
///
/// Do not use `EventStream`, `event::poll`, or `event::read` alongside this reader.
/// Crossterm requires polling and reading on the same thread; its EventStream
/// worker only wakes the consumer, which would timestamp delayed processing
/// rather than receipt. Drop this reader before handing the terminal to a child.
pub struct InputEvents {
    events: Arc<Mutex<InputQueue>>,
    ready: Arc<Notify>,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl InputEvents {
    pub fn new() -> Self {
        let events = Arc::new(Mutex::new(InputQueue::default()));
        let ready = Arc::new(Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));
        let reader_events = Arc::clone(&events);
        let reader_ready = Arc::clone(&ready);
        let reader_stopped = Arc::clone(&stopped);
        let reader = thread::Builder::new()
            .name("terminal-input".into())
            .spawn(move || {
                let mut error = None;
                while !reader_stopped.load(Ordering::Relaxed) {
                    let received = match event::poll(Duration::from_millis(25)) {
                        Ok(false) => {
                            if reader_events.lock().unwrap().observe_quiet(Instant::now()) {
                                reader_ready.notify_one();
                            }
                            continue;
                        }
                        Ok(true) => event::read().map(|event| ReceivedEvent {
                            event,
                            received_at: Instant::now(),
                        }),
                        Err(error) => Err(error),
                    };
                    let ready = match received {
                        Ok(received) => reader_events.lock().unwrap().push(received),
                        Err(failed) => {
                            error = Some(failed);
                            break;
                        }
                    };
                    // A full queue never waits for the consumer: it quarantines
                    // and keeps draining terminal input instead. Discarded input
                    // and idle polls need not wake the UI.
                    if ready {
                        reader_ready.notify_one();
                    }
                }
                reader_events.lock().unwrap().finish(error);
                reader_ready.notify_one();
            })
            .expect("failed to spawn terminal input reader");
        Self {
            events,
            ready,
            stopped,
            reader: Some(reader),
        }
    }

    pub async fn next(&mut self) -> Option<io::Result<InputEvent>> {
        loop {
            // There is one consumer. notify_one retains a permit if the reader
            // signals between this queue check and polling the notification.
            let notified = self.ready.notified();
            {
                let mut queue = self.events.lock().unwrap();
                if let Some(event) = queue.pop() {
                    return Some(event);
                }
                if queue.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub fn try_next(&mut self) -> Option<io::Result<InputEvent>> {
        self.events.lock().unwrap().pop()
    }
}

impl Drop for InputEvents {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tui::{
        app::{Action, App, AttachmentKind, Block},
        transcript::Role,
    };
    use crossterm::{
        event::{KeyCode, KeyEvent, KeyModifiers},
        terminal,
    };
    use std::{
        fs::File,
        io::{BufRead, BufReader, Read, Seek, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{net::UnixStream, process::CommandExt},
        },
        process::{Child, Command, Stdio},
        sync::mpsc as sync_mpsc,
    };

    const CHILD_ENV: &str = "KIT_INPUT_RECEIPT_TEST_CHILD";
    const CONTROL_ENV: &str = "KIT_INPUT_RECEIPT_TEST_CONTROL_FD";

    // Always reap the isolated test, including when a handshake times out.
    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn mark(message: &str) {
        println!("INPUT_TEST:{message}");
        io::stdout().flush().unwrap();
    }

    fn assert_key(received: &ReceivedEvent, expected: char) {
        assert!(
            matches!(&received.event, Event::Key(key) if key.code == KeyCode::Char(expected)),
            "expected {expected:?}, got {received:?}"
        );
    }

    fn received(item: io::Result<InputEvent>) -> ReceivedEvent {
        match item.unwrap() {
            InputEvent::Event(event) => event,
            other => panic!("expected terminal event, got {other:?}"),
        }
    }

    fn key(code: KeyCode, received_at: Instant) -> ReceivedEvent {
        ReceivedEvent {
            event: Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            received_at,
        }
    }

    #[test]
    fn queue_event_and_payload_budgets_release_capacity_on_consumption() {
        let now = Instant::now();
        let mut queue = InputQueue::default();
        for _ in 0..MAX_QUEUED_EVENTS {
            queue.push(key(KeyCode::Char('x'), now));
        }
        for _ in 0..MAX_QUEUED_EVENTS {
            assert_key(&received(queue.pop().unwrap()), 'x');
        }
        assert!(queue.pop().is_none());
        for _ in 0..2 {
            queue.push(ReceivedEvent {
                event: Event::Paste("x".repeat(MAX_QUEUED_BYTES / 2)),
                received_at: now,
            });
        }
        let mut bytes = 0;
        while let Some(item) = queue.pop() {
            let Event::Paste(text) = received(item).event else {
                panic!("expected paste")
            };
            bytes += text.capacity();
        }
        assert_eq!(bytes, MAX_QUEUED_BYTES);
        queue.push(ReceivedEvent {
            event: Event::Paste("y".repeat(MAX_QUEUED_BYTES)),
            received_at: now,
        });
        assert!(matches!(
            received(queue.pop().unwrap()).event,
            Event::Paste(_)
        ));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn overload_discards_pending_controls_and_requires_quiet_acknowledgement() {
        for payload in [false, true] {
            let now = Instant::now();
            let mut queue = InputQueue::default();
            if payload {
                // Repeated bracketed-paste payloads saturate the byte budget
                // long before the event count. Both old payloads must be freed.
                for _ in 0..3 {
                    queue.push(ReceivedEvent {
                        event: Event::Paste("x".repeat(MAX_QUEUED_BYTES / 2)),
                        received_at: now,
                    });
                }
            } else {
                for _ in 0..MAX_QUEUED_EVENTS {
                    queue.push(key(KeyCode::Char('x'), now));
                }
                queue.push(key(KeyCode::Enter, now));
            }
            assert!(matches!(
                queue.pop().unwrap().unwrap(),
                InputEvent::Overflow
            ));
            assert!(queue.pop().is_none());
            // A large timestamp gap alone does not prove the terminal is quiet.
            let later = now + RECOVERY_QUIET * 10;
            for code in [KeyCode::Esc, KeyCode::Tab, KeyCode::Enter] {
                queue.push(key(code, later));
            }
            assert!(queue.pop().is_none());
            queue.observe_quiet(later + RECOVERY_QUIET);
            // Even after quiet, Enter is not acknowledgement and remains inert.
            queue.push(key(KeyCode::Enter, later + RECOVERY_QUIET));
            // Unconsumed readiness is revoked, not left as a stale ready event.
            assert!(matches!(
                queue.pop().unwrap().unwrap(),
                InputEvent::RecoveryReady(false)
            ));
            assert!(queue.pop().is_none());
            queue.observe_quiet(later + RECOVERY_QUIET * 2);
            queue.push(key(KeyCode::Esc, later + RECOVERY_QUIET * 2));
            // A tail after Esc is also discarded, not leaked into the composer.
            queue.push(key(KeyCode::Tab, later + RECOVERY_QUIET * 3));
            queue.push(key(KeyCode::Enter, later + RECOVERY_QUIET * 3));
            queue.observe_quiet(later + RECOVERY_QUIET * 3);
            assert!(matches!(
                queue.pop().unwrap().unwrap(),
                InputEvent::RecoveryReady(false)
            ));
            assert!(queue.pop().is_none());
            queue.observe_quiet(later + RECOVERY_QUIET * 4);
            assert!(matches!(
                queue.pop().unwrap().unwrap(),
                InputEvent::Resumed(_)
            ));
            assert!(queue.pop().is_none());
            queue.push(key(KeyCode::Char('z'), later + RECOVERY_QUIET * 5));
            assert_key(&received(queue.pop().unwrap()), 'z');
        }
    }

    #[test]
    fn composer_partial_paste_cannot_submit_until_overload_is_acknowledged() {
        let mut app = App::new(
            "/tmp".into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.paste("existing draft");
        let now = Instant::now();
        let mut queue = InputQueue::default();
        queue.push(key(KeyCode::Char('x'), now));
        assert!(matches!(
            crate::tui::handle_input(&mut app, queue.pop().unwrap().unwrap()),
            Action::None
        ));
        let partial = app.editor.text().to_owned();
        for _ in 0..MAX_QUEUED_EVENTS {
            queue.push(key(KeyCode::Char('x'), now));
        }
        queue.push(key(KeyCode::Enter, now));
        assert!(matches!(
            crate::tui::handle_input(&mut app, queue.pop().unwrap().unwrap()),
            Action::Redraw
        ));
        queue.observe_quiet(now + RECOVERY_QUIET);
        queue.push(key(KeyCode::Enter, now + RECOVERY_QUIET));
        assert!(matches!(
            crate::tui::handle_input(&mut app, queue.pop().unwrap().unwrap()),
            Action::Redraw
        ));
        assert!(!app.input_recovery_ready);
        assert!(queue.pop().is_none());
        assert_eq!(app.editor.text(), partial);
        assert!(app.input_overflow);
        queue.observe_quiet(now + RECOVERY_QUIET * 2);
        queue.push(key(KeyCode::Esc, now + RECOVERY_QUIET * 2));
        queue.push(key(KeyCode::Enter, now + RECOVERY_QUIET * 2));
        queue.observe_quiet(now + RECOVERY_QUIET * 3);
        assert!(matches!(
            crate::tui::handle_input(&mut app, queue.pop().unwrap().unwrap()),
            Action::Redraw
        ));
        assert_eq!(app.editor.text(), partial);
        assert!(!app.input_overflow);
        // Fresh input works after acknowledgement; an accompanying pasted
        // newline still inserts text, rather than submitting a partial draft.
        for code in [KeyCode::Char('z'), KeyCode::Enter] {
            queue.push(key(code, now + RECOVERY_QUIET * 4));
            assert!(matches!(
                crate::tui::handle_input(&mut app, queue.pop().unwrap().unwrap()),
                Action::None
            ));
        }
        assert!(app.editor.text().starts_with(&partial));
        assert!(app.editor.text().ends_with("z\n"));
    }

    #[test]
    fn oversized_paste_spare_capacity_is_not_retained() {
        let mut queue = InputQueue::default();
        let mut text = String::with_capacity(MAX_QUEUED_BYTES + 1);
        text.push('x');
        queue.push(ReceivedEvent {
            event: Event::Paste(text),
            received_at: Instant::now(),
        });
        assert!(matches!(
            queue.pop().unwrap().unwrap(),
            InputEvent::Overflow
        ));
        for _ in 0..20 {
            queue.push(ReceivedEvent {
                event: Event::Paste("x".repeat(MAX_QUEUED_BYTES + 1)),
                received_at: Instant::now(),
            });
        }
        assert!(queue.pop().is_none());
        queue.finish(None);
        assert!(queue.closed);
    }

    #[test]
    fn recovery_readiness_is_coalesced_and_reported_only_on_transitions() {
        let mut queue = InputQueue::default();
        let mut now = Instant::now();
        queue.push(ReceivedEvent {
            event: Event::Paste("x".repeat(MAX_QUEUED_BYTES + 1)),
            received_at: now,
        });
        assert!(matches!(
            queue.pop().unwrap().unwrap(),
            InputEvent::Overflow
        ));
        queue.observe_quiet(now + RECOVERY_QUIET);
        assert!(matches!(
            queue.pop().unwrap().unwrap(),
            InputEvent::RecoveryReady(true)
        ));
        queue.observe_quiet(now + RECOVERY_QUIET * 2);
        assert!(queue.pop().is_none()); // No idle notification loop.
        now += RECOVERY_QUIET * 2;
        for _ in 0..100 {
            now += RECOVERY_QUIET;
            queue.push(key(KeyCode::Enter, now));
            queue.observe_quiet(now + RECOVERY_QUIET);
        }
        // Only the latest state survives slow UI consumption, not 200 messages.
        assert!(matches!(
            queue.pop().unwrap().unwrap(),
            InputEvent::RecoveryReady(true)
        ));
        assert!(queue.pop().is_none());
        queue.push(key(KeyCode::Tab, now + RECOVERY_QUIET));
        assert!(matches!(
            queue.pop().unwrap().unwrap(),
            InputEvent::RecoveryReady(false)
        ));
        assert!(queue.pop().is_none());
        queue.finish(None);
        queue.observe_quiet(now + RECOVERY_QUIET * 3);
        assert!(queue.pop().is_none());
    }

    async fn await_recovery(input: &mut InputEvents, app: &mut App, phase: &str, resume: bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = input.next().await.expect("reader remains open").unwrap();
                let complete = match &event {
                    InputEvent::RecoveryReady(ready) => *ready && !resume,
                    InputEvent::Resumed(_) if resume => true,
                    other => panic!("{phase}: unexpected recovery event {other:?}"),
                };
                crate::tui::handle_input(app, event);
                if complete {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{phase}: reader did not report recovery (resume={resume})"));
    }

    fn run_child() {
        // SAFETY: only the isolated child executes this function; pre_exec gives
        // it ownership of this test-control descriptor, separate from the PTY.
        let control_fd = std::env::var(CONTROL_ENV).unwrap().parse().unwrap();
        let mut control = unsafe { File::from_raw_fd(control_fd) };
        terminal::enable_raw_mode().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut input = InputEvents::new();
                mark("ready");
                let mut app = App::new(
                    "/tmp".into(),
                    "provider".into(),
                    "model".into(),
                    "a2a".into(),
                );
                app.blocks
                    .push(Block::User("ab matching history".to_owned().into()));
                app.sync_transcript_cache();
                app.paste("parked draft");
                app.attach(
                    "/tmp/parked.png".into(),
                    "image/png",
                    AttachmentKind::Image,
                    12,
                );
                let draft = app.editor.text().to_owned();
                let attachments = app.attachments.clone();
                app.open_navigation();
                let first = received(input.next().await.unwrap());
                assert_key(&first, 'a');
                assert!(matches!(crate::tui::handle(&mut app, first), Action::None));
                mark("processing");

                // Block even the async executor: only the production reader can
                // capture the subsequent bytes while the TUI is busy.
                thread::sleep(Duration::from_millis(500));
                let resumed_at = Instant::now();
                let b = received(input.try_next().expect("b captured while busy"));
                let tab = received(input.try_next().expect("Tab captured while busy"));
                let pasted_enter =
                    received(input.try_next().expect("paste Enter captured while busy"));
                let second_enter = received(
                    input
                        .try_next()
                        .expect("second paste Enter captured while busy"),
                );
                let deliberate_enter = received(
                    input
                        .try_next()
                        .expect("deliberate Enter captured while busy"),
                );
                assert_key(&b, 'b');
                assert!(
                    resumed_at.duration_since(deliberate_enter.received_at)
                        > Duration::from_millis(100)
                );
                assert!(tab.received_at.duration_since(b.received_at) > Duration::from_millis(8));
                assert!(
                    pasted_enter.received_at.duration_since(tab.received_at)
                        < Duration::from_millis(8)
                );
                assert!(matches!(crate::tui::handle(&mut app, b), Action::None));
                assert!(matches!(crate::tui::handle(&mut app, tab), Action::None));
                assert_eq!(app.navigation.dialog.as_ref().unwrap().role, Role::User);
                for event in [pasted_enter, second_enter] {
                    assert!(matches!(crate::tui::handle(&mut app, event), Action::None));
                    assert!(app.navigation.dialog.is_some());
                    assert_eq!(app.editor.text(), draft);
                }
                assert!(matches!(
                    crate::tui::handle(&mut app, deliberate_enter),
                    Action::None
                ));
                assert!(app.navigation.dialog.is_none());
                assert!(app.navigation.revealed.is_some());
                assert_eq!(app.editor.text(), draft);
                assert!(input.try_next().is_none());

                for kind in ["keys", "bytes"] {
                    app.open_navigation();
                    mark(&format!("flood-{kind}"));
                    // A separate test-transport pipe keeps the consumer blocked
                    // until the parent finishes writing the entire PTY flood.
                    // This does not assume a fixed terminal-decoding throughput.
                    control.read_exact(&mut [0]).unwrap();
                    thread::sleep(Duration::from_millis(100));
                    let overflow = input.next().await.unwrap().unwrap();
                    assert!(matches!(overflow, InputEvent::Overflow));
                    assert!(matches!(
                        crate::tui::handle_input(&mut app, overflow),
                        Action::Redraw
                    ));
                    assert!(app.input_overflow);
                    assert!(app.navigation.dialog.is_some());
                    assert_eq!(app.editor.text(), draft);
                    assert_eq!(app.attachments, attachments);
                    await_recovery(&mut input, &mut app, &format!("flood-{kind}"), false).await;
                    assert!(app.input_recovery_ready);
                    mark(&format!("blocked-{kind}"));
                    // The parent sends controls and a premature Esc. Wait for
                    // the reader's next quiet transition, not a scheduling delay.
                    await_recovery(&mut input, &mut app, &format!("blocked-{kind}"), false).await;
                    assert!(app.input_overflow);
                    assert!(app.input_recovery_ready);
                    mark(&format!("ack-{kind}"));
                    await_recovery(&mut input, &mut app, &format!("ack-{kind}"), true).await;
                    assert!(!app.input_overflow);
                    assert!(app.navigation.dialog.is_some()); // Esc did not close it.
                    assert_eq!(app.editor.text(), draft);
                    assert_eq!(app.attachments, attachments);
                    mark(&format!("recover-{kind}"));
                    for _ in 0..3 {
                        let event = input.next().await.unwrap().unwrap();
                        assert!(matches!(
                            crate::tui::handle_input(&mut app, event),
                            Action::None
                        ));
                    }
                    assert!(app.navigation.dialog.is_none());
                    assert!(app.navigation.revealed.is_some());
                    assert_eq!(app.editor.text(), draft);
                    assert_eq!(app.attachments, attachments);
                }

                let dropping_at = Instant::now();
                drop(input);
                // Allow scheduling headroom, but catch a blocking read on an
                // idle terminal instead of the bounded poll.
                assert!(dropping_at.elapsed() < Duration::from_secs(1));
                let mut input = InputEvents::new();
                mark("restarted");
                assert_key(&received(input.next().await.unwrap()), 'e');
                mark("fill-before-drop");
                thread::sleep(Duration::from_millis(500));
                let dropping_at = Instant::now();
                drop(input);
                assert!(dropping_at.elapsed() < Duration::from_secs(1));
            });
        terminal::disable_raw_mode().unwrap();
        mark("done");
    }

    #[test]
    fn pty_captures_receipt_times_while_busy_and_restarts() {
        run_pty_test(false);
    }

    // Exercise the Linux polling backend's /dev/tty fallback. The unchanged
    // macOS MIO backend cannot initialize with redirected stdin on all systems.
    #[cfg(target_os = "linux")]
    #[test]
    fn pty_uses_controlling_terminal_with_redirected_stdin() {
        run_pty_test(true);
    }

    fn run_pty_test(redirect_stdin: bool) {
        if std::env::var_os(CHILD_ENV).is_some() {
            run_child();
            return;
        }

        let (mut master_fd, mut slave_fd) = (-1, -1);
        // SAFETY: openpty initializes both descriptors; optional settings are
        // null, and ownership transfers exactly once to the File values below.
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
            io::Error::last_os_error()
        );
        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let (mut control, child_control) = UnixStream::pair().unwrap();
        let mut redirected = redirect_stdin.then(|| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(b"not terminal input\r\t\r").unwrap();
            file.rewind().unwrap();
            file
        });
        let redirected_fd = redirected.as_ref().map(AsRawFd::as_raw_fd);
        let mut command = Command::new(std::env::current_exe().unwrap());
        let test_name = thread::current().name().unwrap().to_owned();
        command
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD_ENV, "1")
            .env(CONTROL_ENV, child_control.as_raw_fd().to_string())
            .stdin(Stdio::from(slave))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: only async-signal-safe terminal setup runs between fork and
        // exec. Stdin initially supplies the controlling PTY, not the developer's
        // terminal. Redirecting it afterwards exercises the /dev/tty fallback.
        // The extra descriptor is only a test-transport completion signal.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1
                    || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1
                    || libc::fcntl(child_control.as_raw_fd(), libc::F_SETFD, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                if let Some(fd) = redirected_fd
                    && libc::dup2(fd, libc::STDIN_FILENO) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        drop(command);
        let stdout = child.0.stdout.take().unwrap();
        let (sender, messages) = sync_mpsc::channel();
        let output_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if let Some((_, message)) = line.split_once("INPUT_TEST:") {
                    let _ = sender.send(message.to_owned());
                }
            }
        });
        let wait_for = |expected: &str| {
            assert_eq!(
                messages.recv_timeout(Duration::from_secs(10)).unwrap(),
                expected
            );
        };
        wait_for("ready");
        master.write_all(b"a").unwrap();
        wait_for("processing");
        master.write_all(b"b").unwrap();
        thread::sleep(Duration::from_millis(80));
        master.write_all(b"\t\r\r").unwrap();
        thread::sleep(Duration::from_millis(80));
        master.write_all(b"\r").unwrap();
        for kind in ["keys", "bytes"] {
            wait_for(&format!("flood-{kind}"));
            if kind == "keys" {
                master
                    .write_all(&vec![b'x'; MAX_QUEUED_EVENTS * 8])
                    .unwrap();
            } else {
                for _ in 0..4 {
                    master.write_all(b"\x1b[200~").unwrap();
                    master.write_all(&vec![b'x'; MAX_QUEUED_BYTES / 2]).unwrap();
                    master.write_all(b"\x1b[201~").unwrap();
                }
            }
            master.write_all(b"\r\t\r").unwrap();
            control.write_all(b"!").unwrap();
            wait_for(&format!("blocked-{kind}"));
            master.write_all(b"\r\t\r\x1b").unwrap();
            wait_for(&format!("ack-{kind}"));
            master.write_all(b"\x1b").unwrap();
            wait_for(&format!("recover-{kind}"));
            master.write_all(b"ab").unwrap();
            thread::sleep(Duration::from_millis(80));
            master.write_all(b"\r").unwrap();
        }
        wait_for("restarted");
        master.write_all(b"e").unwrap();
        wait_for("fill-before-drop");
        master.write_all(&vec![b'x'; MAX_QUEUED_EVENTS]).unwrap();
        wait_for("done");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "PTY child failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "PTY child failed to exit");
            thread::sleep(Duration::from_millis(10));
        }
        output_reader.join().unwrap();
        if let Some(file) = redirected.as_mut() {
            assert_eq!(
                file.stream_position().unwrap(),
                0,
                "redirected input consumed"
            );
        }
    }
}
