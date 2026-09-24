//! Blocking work whose result is optional when the terminal session ends.

use tokio::sync::oneshot;

/// Run bounded blocking work independently of the async runtime's shutdown.
///
/// Unlike `spawn_blocking`, dropping the runtime never joins this worker. Dropping
/// the receiver discards its result; it does not interrupt in-flight work. Callers
/// must bound the work itself (usage HTTP requests have finite timeouts and body
/// limits), and retain at most one pending check per session. Process exit does
/// not wait for these detached threads.
pub(super) fn spawn<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<oneshot::Receiver<T>> {
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name("kit-usage".into())
        .spawn(move || {
            let _ = sender.send(work());
        })?;
    Ok(receiver)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    #[test]
    fn runtime_shutdown_does_not_wait_for_pending_work() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let result = {
            let _entered = runtime.enter();
            spawn(move || {
                gate.recv().unwrap();
                "finished"
            })
            .unwrap()
        };
        // The work cannot finish before shutdown: no sleeps, deadlines, or
        // scheduler assumptions. A Tokio blocking-pool worker would deadlock.
        drop(runtime);
        release.send(()).unwrap();
        assert_eq!(result.blocking_recv().unwrap(), "finished");
    }

    #[test]
    fn abandoning_result_does_not_prevent_worker_cleanup() {
        let (release, gate) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let result = spawn(move || {
            gate.recv().unwrap();
            finished.send(()).unwrap();
        })
        .unwrap();
        drop(result);
        release.send(()).unwrap();
        done.recv().unwrap();
    }
}
