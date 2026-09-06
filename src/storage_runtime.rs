//! Process-lifetime recovery and shutdown coordination for internal storage.

use std::{io::Write as _, sync::OnceLock, time::Duration};

use tokio_util::sync::CancellationToken;

static SHUTDOWN: OnceLock<CancellationToken> = OnceLock::new();
static WORKER: OnceLock<()> = OnceLock::new();

pub fn shutdown_token() -> &'static CancellationToken {
    SHUTDOWN.get_or_init(CancellationToken::new)
}

pub fn request_shutdown() {
    shutdown_token().cancel();
}

/// Actual allocator failure cannot safely reach callers that format errors.
///
/// Use only static writes and best-effort terminal cleanup, then skip destructors
/// and runtime teardown. This is not the configurable overlay-budget exit path.
fn noallocation_failure_exit() -> ! {
    crate::tui::restore_after_allocation_failure();
    let _ = std::io::stderr().write_all(
        b"kit: memory allocation failed; exiting immediately. Unpersisted internal storage changes will be lost.\n",
    );
    std::process::exit(1);
}

/// Stop background work and make one final bounded-queue recovery pass.
///
/// Call synchronously after command work ends, before returning from main. This
/// does not wait for storage capacity to return or retry indefinitely. Passing
/// the service explicitly also permits isolated fault-injection tests.
pub fn finish_recovery(filesystem: &crate::resilient_fs::Fs) -> std::io::Result<()> {
    request_shutdown();
    let _ = filesystem.recover();
    let status = filesystem.status();
    if status.pending_operations > 0 {
        let _ = std::io::stderr().write_all(
            b"kit: exiting with unpersisted internal storage changes; pending in-memory data will be lost at process exit.\n",
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "pending internal storage changes could not be persisted before exit",
        ));
    }
    if status.exhausted {
        let _ = std::io::stderr().write_all(
            b"kit: internal storage memory budget was exhausted; not all requested changes were accepted.\n",
        );
        return Err(std::io::Error::other(
            "internal storage budget exhausted before exit",
        ));
    }
    Ok(())
}

/// Retry the optional storage domain without turning loss into process failure.
/// The service, not this worker, owns buffering, ordering, and replay. No worker
/// state is shared: only Fs's existing synchronization coordinates concurrent
/// submission, recovery, and the final pass. Abandonment releases queued images
/// and leases outside the filesystem guard; poison is never recovered.
fn recover_best_effort(
    filesystem: &crate::resilient_fs::Fs,
) -> Option<crate::resilient_fs::BestEffortStatus> {
    use crate::resilient_fs::BestEffortStatus;
    match filesystem.best_effort_status()? {
        BestEffortStatus::Ready | BestEffortStatus::Buffered => {
            let _ = filesystem.recover();
        }
        BestEffortStatus::Dropped | BestEffortStatus::Poisoned => {}
    }
    if filesystem.best_effort_status() == Some(BestEffortStatus::Dropped) {
        let _ = filesystem.abandon_best_effort();
    }
    filesystem.best_effort_status()
}

fn report_best_effort(
    status: Option<crate::resilient_fs::BestEffortStatus>,
    previous: &mut Option<crate::resilient_fs::BestEffortStatus>,
) {
    use crate::resilient_fs::BestEffortStatus;
    if status == *previous {
        return;
    }
    let message: Option<&[u8]> = match status {
        Some(BestEffortStatus::Buffered) => Some(
            b"kit: transcript storage is buffered in memory; history may be lost if Kit exits before storage recovers.\n",
        ),
        Some(BestEffortStatus::Dropped | BestEffortStatus::Poisoned) => Some(
            b"kit: transcript persistence is disabled for this process; the agent will continue, but new history will not be saved.\n",
        ),
        Some(BestEffortStatus::Ready) if *previous == Some(BestEffortStatus::Buffered) => Some(
            b"kit: transcript storage recovered; pending writes have been replayed.\n",
        ),
        _ => None,
    };
    if let Some(message) = message {
        let _ = std::io::stderr().write_all(message);
    }
    *previous = status;
}

/// Make a final bounded attempt for optional transcripts. Unpersisted history
/// is permitted to be lost and must not replace the command's exit result.
pub fn finish_best_effort_recovery(filesystem: &crate::resilient_fs::Fs) {
    let status = recover_best_effort(filesystem);
    report_best_effort(status, &mut None);
}

/// Start one recovery worker for the process, independent of session handles.
/// A turn completing or a session closing does not stop this worker or discard
/// the filesystem's pending changes.
pub fn start_recovery_worker() {
    // Register before starting any worker or initializing its filesystem. An
    // existing application-provided handler takes precedence.
    let _ = crate::resilient_fs::set_allocation_failure_handler(noallocation_failure_exit);
    WORKER.get_or_init(|| {
        let worker = std::thread::Builder::new()
            .name("kit-storage-recovery".into())
            .spawn(|| {
                let fs = crate::resilient_fs::global();
                let best_effort = crate::resilient_fs::best_effort_global();
                let mut best_effort_status = None;
                let mut delay = 1;
                let mut warned = false;
                let mut emitted_status = None;
                loop {
                    let best_status = recover_best_effort(best_effort);
                    report_best_effort(best_status, &mut best_effort_status);
                    let best_pending = best_status == Some(crate::resilient_fs::BestEffortStatus::Buffered);
                    let status = fs.status();
                    publish_status(status.pending_operations > 0 || best_pending, status.exhausted, &mut emitted_status);
                    if status.exhausted {
                        let _ = std::io::stderr().write_all(
                            b"kit: internal storage memory budget exhausted; cancelling work and shutting down. Unpersisted data cannot survive process exit.\n",
                        );
                        request_shutdown();
                    }
                    if status.pending_operations > 0 {
                        if !warned {
                            let _ = std::io::stderr().write_all(
                                b"kit: internal storage is temporarily retained in memory; persistence will resume when disk storage recovers.\n",
                            );
                            warned = true;
                        }
                        let _ = fs.recover();
                        let recovered = fs.status();
                        publish_status(recovered.pending_operations > 0 || best_pending, recovered.exhausted, &mut emitted_status);
                        if recovered.pending_operations == 0 {
                            let _ = std::io::stderr().write_all(
                                b"kit: internal storage recovered; pending changes are persisted.\n",
                            );
                            warned = false;
                            delay = 1;
                        } else {
                            delay = (delay * 2).min(30);
                        }
                    } else {
                        if warned {
                            let _ = std::io::stderr().write_all(
                                b"kit: internal storage recovered; pending changes are persisted.\n",
                            );
                            warned = false;
                        }
                        delay = 1;
                    }
                    if shutdown_token().is_cancelled() {
                        // One final best-effort pass; never discard pending data
                        // merely because a caller or observer was dropped.
                        let _ = fs.recover();
                        finish_best_effort_recovery(best_effort);
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(delay));
                }
            });
        if worker.is_err() {
            let _ = std::io::stderr().write_all(
                b"kit: could not start internal storage recovery; shutting down.\n",
            );
            request_shutdown();
        }
    });
}

fn publish_status(pending: bool, exhausted: bool, previous: &mut Option<(bool, bool)>) {
    if *previous != Some((pending, exhausted)) {
        crate::events::emit(&crate::events::RuntimeEvent::StorageStatus { pending, exhausted });
        *previous = Some((pending, exhausted));
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    #[test]
    fn optional_storage_loss_does_not_exhaust_strict_storage_or_fail_final_pass() {
        use crate::resilient_fs::{BestEffortStatus, DiskBackend, Fs};
        let directory = tempfile::tempdir().unwrap();
        let strict = Fs::new(std::sync::Arc::new(DiskBackend));
        let optional = strict.best_effort(0, 0);
        // A successful immediate disk write needs no buffer budget. Exercise
        // explicit loss through the production abandonment boundary instead.
        optional
            .write(directory.path().join("transcript"), b"prefix")
            .unwrap();
        optional.abandon_best_effort().unwrap();
        assert!(
            optional
                .write(directory.path().join("transcript"), b"lost tail")
                .is_err()
        );
        assert_eq!(
            optional.best_effort_status(),
            Some(BestEffortStatus::Dropped)
        );
        super::finish_best_effort_recovery(&optional);
        assert_eq!(optional.status().pending_operations, 0);
        assert_eq!(optional.status().retained_bytes, 0);
        assert!(!strict.status().exhausted);
        strict
            .write(directory.path().join("strict"), b"important")
            .unwrap();
        strict
            .require_disk(directory.path().join("strict"))
            .unwrap();
        assert_eq!(
            strict.read(directory.path().join("strict")).unwrap(),
            b"important"
        );
    }

    #[test]
    fn optional_recovery_does_not_operate_on_a_strict_service() {
        let strict =
            crate::resilient_fs::Fs::new(std::sync::Arc::new(crate::resilient_fs::DiskBackend));
        assert_eq!(super::recover_best_effort(&strict), None);
        let mut previous = None;
        super::report_best_effort(None, &mut previous);
        assert_eq!(previous, None);
    }

    #[test]
    fn allocation_failure_exit_is_nonzero_without_protocol_stdout() {
        const CHILD: &str = "KIT_ALLOCATION_EXIT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            super::noallocation_failure_exit();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage_runtime::tests::allocation_failure_exit_is_nonzero_without_protocol_stdout",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            output.stderr,
            b"kit: memory allocation failed; exiting immediately. Unpersisted internal storage changes will be lost.\n"
        );
        // The test harness writes its own banner, but no terminal escapes may
        // contaminate a process that never entered the TUI.
        assert!(!output.stdout.contains(&0x1b));
    }
}
