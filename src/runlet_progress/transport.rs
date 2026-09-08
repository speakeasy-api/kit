//! One process-owned blocking writer, never joined by an execution future.
//! Publication is bounded and nonblocking, including cancellation/Drop paths.
use super::Progress;
use crate::events::{EVENT_MARKER, RuntimeEvent};
use std::{
    io::{self, Write},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::Duration,
};

const CAPACITY: usize = 256;
const HEARTBEAT: Duration = Duration::from_millis(500);
pub(crate) const LEASE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct Transport {
    sender: SyncSender<Progress>,
    disabled: Arc<AtomicBool>,
}
impl Transport {
    /// A concrete owned IO boundary; production uses stderr, tests may use a pipe.
    pub(crate) fn start(writer: impl Write + Send + 'static, capacity: usize) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let disabled = Arc::new(AtomicBool::new(false));
        let guard = WorkerGuard(disabled.clone());
        std::thread::Builder::new()
            .name("runlet-diagnostics".into())
            .spawn(move || {
                // Guard invalidates publication on success, error, or unwind. No IO
                // in its destructor and no restart that could reuse stale evidence.
                let _guard = guard;
                let _ = write_loop(writer, receiver, &_guard.0);
            })?;
        Ok(Self { sender, disabled })
    }
    pub(crate) fn publish(&self, progress: Progress) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        if !progress.bounded() || self.sender.try_send(progress).is_err() {
            self.disabled.store(true, Ordering::Release);
        }
    }
}
struct WorkerGuard(Arc<AtomicBool>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) fn global() -> Option<&'static Transport> {
    static TRANSPORT: OnceLock<Option<Transport>> = OnceLock::new();
    TRANSPORT
        .get_or_init(|| Transport::start(std::io::stderr(), CAPACITY).ok())
        .as_ref()
}

fn write_loop(
    mut writer: impl Write,
    receiver: Receiver<Progress>,
    disabled: &AtomicBool,
) -> io::Result<()> {
    write_frame(
        &mut writer,
        &RuntimeEvent::RunletTransport { available: true },
    )?;
    loop {
        if disabled.load(Ordering::Acquire) {
            // Do not enqueue reset behind stale frames or assume it was delivered.
            // Failure/blocking here is covered by the client's monotonic lease.
            return write_frame(
                &mut writer,
                &RuntimeEvent::RunletTransport { available: false },
            );
        }
        match receiver.recv_timeout(HEARTBEAT) {
            Ok(progress) => {
                if disabled.load(Ordering::Acquire) {
                    continue;
                }
                write_frame(&mut writer, &RuntimeEvent::RunletProgress { progress })?;
                // Even a busy queue must renew liveness. Progress frames also
                // renew the lease; idle liveness is sent on recv_timeout below.
            }
            Err(RecvTimeoutError::Timeout) => write_frame(
                &mut writer,
                &RuntimeEvent::RunletTransport { available: true },
            )?,
            Err(RecvTimeoutError::Disconnected) => {
                return write_frame(
                    &mut writer,
                    &RuntimeEvent::RunletTransport { available: false },
                );
            }
        }
    }
}
fn write_frame(writer: &mut impl Write, event: &RuntimeEvent) -> io::Result<()> {
    let mut line = EVENT_MARKER.as_bytes().to_vec();
    serde_json::to_writer(&mut line, event).map_err(io::Error::other)?;
    line.push(b'\n');
    // One write_all call also gives stderr one whole-frame lock, avoiding
    // interleaving with the existing synchronous diagnostic publishers.
    writer.write_all(&line)
}

#[cfg(test)]
mod tests;
