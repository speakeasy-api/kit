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
    time::{Duration, Instant},
};

const CAPACITY: usize = 256;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const HEARTBEAT: Duration = Duration::from_millis(500);
pub(crate) const LEASE: Duration = Duration::from_secs(5);

enum Frame {
    Authoritative(Vec<u8>),
    Diagnostic(Vec<u8>),
}

#[derive(Clone)]
pub(crate) struct Transport {
    sender: SyncSender<Frame>,
    disabled: Arc<AtomicBool>,
    diagnostics_lost: Arc<AtomicBool>,
}
impl Transport {
    /// A concrete owned IO boundary; production uses stderr, tests may use a pipe.
    pub(crate) fn start(
        writer: impl Write + Send + 'static,
        capacity: usize,
        runtime_events: bool,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let disabled = Arc::new(AtomicBool::new(false));
        let guard = WorkerGuard(disabled.clone());
        let diagnostics_lost = Arc::new(AtomicBool::new(false));
        let worker_loss = diagnostics_lost.clone();
        std::thread::Builder::new()
            .name("runlet-diagnostics".into())
            .spawn(move || {
                // Guard invalidates publication on success, error, or unwind. No IO
                // in its destructor and no restart that could reuse stale evidence.
                let _guard = guard;
                let _ = write_loop(writer, receiver, &_guard.0, &worker_loss, runtime_events);
            })?;
        Ok(Self {
            sender,
            disabled,
            diagnostics_lost,
        })
    }
    pub(crate) fn publish(&self, progress: Progress) {
        self.publish_event(&RuntimeEvent::RunletProgress { progress });
    }

    /// Best-effort diagnostics remain available after authoritative loss.
    pub(crate) fn publish_line(&self, line: &str) {
        const TRUNCATED: &str = " [truncated]";
        let mut frame = Vec::with_capacity(line.len().saturating_add(1).min(MAX_FRAME_BYTES));
        if line.len() >= MAX_FRAME_BYTES {
            let end = line.floor_char_boundary(MAX_FRAME_BYTES - TRUNCATED.len() - 1);
            frame.extend_from_slice(&line.as_bytes()[..end]);
            frame.extend_from_slice(TRUNCATED.as_bytes());
        } else {
            frame.extend_from_slice(line.as_bytes());
        }
        frame.push(b'\n');
        if self.sender.try_send(Frame::Diagnostic(frame)).is_err() {
            self.diagnostics_lost.store(true, Ordering::Release);
        }
    }

    /// Forwarded lifecycle frames must never silently cross a loss gap.
    pub(crate) fn publish_runtime_line(&self, line: &str) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        if line.len() >= MAX_FRAME_BYTES {
            self.disabled.store(true, Ordering::Release);
            return;
        }
        let mut frame = Vec::with_capacity(line.len() + 1);
        frame.extend_from_slice(line.as_bytes());
        frame.push(b'\n');
        self.enqueue_authoritative(frame);
    }

    fn enqueue_authoritative(&self, frame: Vec<u8>) {
        if self.sender.try_send(Frame::Authoritative(frame)).is_err() {
            self.disabled.store(true, Ordering::Release);
        }
    }

    /// Loss of any lifecycle frame invalidates authoritative observation too.
    /// Never wait for the sink, queue capacity, or a reset acknowledgement.
    pub(crate) fn publish_event(&self, event: &RuntimeEvent) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        match encode_frame(event) {
            Ok(frame) => self.enqueue_authoritative(frame),
            Err(_) => self.disabled.store(true, Ordering::Release),
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
        .get_or_init(|| {
            Transport::start(std::io::stderr(), CAPACITY, crate::events::enabled()).ok()
        })
        .as_ref()
}

fn write_loop(
    mut writer: impl Write,
    receiver: Receiver<Frame>,
    disabled: &AtomicBool,
    diagnostics_lost: &AtomicBool,
    runtime_events: bool,
) -> io::Result<()> {
    let mut heartbeat = Instant::now();
    transport_status(&mut writer, runtime_events, true)?;
    let mut reset_sent = false;
    loop {
        if disabled.load(Ordering::Acquire) && !reset_sent {
            // A blocked or failed reset is covered by the client lease.
            transport_status(&mut writer, runtime_events, false)?;
            reset_sent = true;
        }
        if diagnostics_lost.swap(false, Ordering::AcqRel) {
            writer.write_all(b"kit: some child diagnostics were dropped\n")?;
        }
        match receiver.recv_timeout(HEARTBEAT) {
            Ok(frame) => {
                let bytes = match frame {
                    Frame::Authoritative(_) if disabled.load(Ordering::Acquire) => continue,
                    Frame::Authoritative(bytes) | Frame::Diagnostic(bytes) => bytes,
                };
                writer.write_all(&bytes)?;
                // Busy legacy diagnostic traffic must not starve the lease.
                if !disabled.load(Ordering::Acquire) && heartbeat.elapsed() >= HEARTBEAT {
                    transport_status(&mut writer, runtime_events, true)?;
                    heartbeat = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if disabled.load(Ordering::Acquire) {
                    continue;
                }
                transport_status(&mut writer, runtime_events, true)?;
                heartbeat = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                return transport_status(&mut writer, runtime_events && !reset_sent, false);
            }
        }
    }
}

fn transport_status(writer: &mut impl Write, enabled: bool, available: bool) -> io::Result<()> {
    if enabled {
        write_frame(writer, &RuntimeEvent::RunletTransport { available })?;
    }
    Ok(())
}

fn write_frame(writer: &mut impl Write, event: &RuntimeEvent) -> io::Result<()> {
    // One whole-frame stderr lock, owned only by the detached worker.
    writer.write_all(&encode_frame(event)?)
}

fn encode_frame(event: &RuntimeEvent) -> io::Result<Vec<u8>> {
    if let RuntimeEvent::RunletProgress { progress } = event
        && !progress.bounded()
    {
        return Err(io::Error::other("invalid progress metadata"));
    }
    // A bounded writer, not an unbounded serialization followed by a size check.
    let mut frame = vec![0; MAX_FRAME_BYTES];
    let len = {
        let mut cursor = io::Cursor::new(frame.as_mut_slice());
        cursor.write_all(EVENT_MARKER.as_bytes())?;
        serde_json::to_writer(&mut cursor, event).map_err(io::Error::other)?;
        cursor.write_all(b"\n")?;
        cursor.position() as usize
    };
    frame.truncate(len);
    Ok(frame)
}

#[cfg(test)]
mod tests;
