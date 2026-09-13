//! One process-owned blocking writer, never joined by an execution future.
//! Publication is bounded and nonblocking, including cancellation/Drop paths.
use super::Progress;
use crate::events::{BoundaryState, EVENT_MARKER, RuntimeEvent};
use std::{
    io::{self, Write},
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};

const CAPACITY: usize = 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const ROTATE: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_millis(500);
pub(crate) const LEASE: Duration = Duration::from_secs(5);

enum Frame {
    Authoritative { generation: u64, bytes: Vec<u8> },
    Diagnostic(Vec<u8>),
}

#[derive(Clone)]
pub(crate) struct Transport {
    source: String,
    gate: Arc<PublicationGate>,
    sender: SyncSender<Frame>,
    terminal: Arc<AtomicBool>,
    admission: Arc<AtomicU64>,
    diagnostics_lost: Arc<AtomicBool>,
}
// Producers only try_read. The worker only try_write for healthy rotation;
// neither path waits for another lock holder. The unit lock owns publication
// claims, not recoverable data. Poison is terminal, never repaired.
#[derive(Default)]
struct PublicationGate {
    active: RwLock<()>,
    lost: AtomicBool,
}

impl Transport {
    /// A concrete owned IO boundary; production uses stderr, tests may use a pipe.
    pub(crate) fn start(
        writer: impl Write + Send + 'static,
        capacity: usize,
        runtime_events: bool,
    ) -> io::Result<Self> {
        if capacity > CAPACITY {
            return Err(io::Error::other("runtime queue exceeds bounded capacity"));
        }
        // Random stream identity is never reused across workers or restarts.
        let mut random = [0; 32];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let source = blake3::Hash::from_bytes(random).to_hex().to_string();
        let worker_source = source.clone();
        let gate = Arc::new(PublicationGate::default());
        let worker_gate = gate.clone();
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let terminal = Arc::new(AtomicBool::new(false));
        let guard = WorkerGuard(terminal.clone());
        let admission = Arc::new(AtomicU64::new(0));
        let worker_admission = admission.clone();
        let diagnostics_lost = Arc::new(AtomicBool::new(false));
        let worker_loss = diagnostics_lost.clone();
        std::thread::Builder::new()
            .name("runlet-diagnostics".into())
            .spawn(move || {
                // Guard invalidates publication on success, error, or unwind. No IO
                // in its destructor and no restart that could reuse stale evidence.
                let _guard = guard;
                let _ = write_loop(
                    writer,
                    receiver,
                    &worker_loss,
                    &worker_admission,
                    runtime_events,
                    &worker_source,
                    &worker_gate,
                );
            })?;
        Ok(Self {
            source,
            gate,
            sender,
            terminal,
            admission,
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
        let Ok(_claim) = self.gate.active.try_read() else {
            self.gate.lost.store(true, Ordering::Release);
            return;
        };
        let Some(generation) = self.generation() else {
            return;
        };
        // Capture authority before parsing and re-encoding, preserving every
        // descendant's provenance. Malformed marked frames fail closed rather
        // than becoming diagnostics, even when a rotation is in progress.
        if let Some(event) = crate::events::parse(line) {
            match encode_scoped(&self.source, generation, &event) {
                Ok(frame) => self.enqueue_authoritative(generation, frame),
                Err(_) => self.lose(generation),
            }
        } else {
            self.lose(generation);
        }
    }

    // Even generations admit; odd generations are closed until the writer has
    // written an ordered Lost/Open boundary. Tokens are captured BEFORE encoding.
    fn generation(&self) -> Option<u64> {
        let generation = self.admission.load(Ordering::Acquire);
        (!self.terminal.load(Ordering::Acquire) && generation.is_multiple_of(2))
            .then_some(generation)
    }

    fn lose(&self, generation: u64) {
        // A late failure from an invalidated producer must not close a newer
        // generation. Concurrent losses of the same generation coalesce.
        let _ = self.admission.compare_exchange(
            generation,
            generation | 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn enqueue_authoritative(&self, generation: u64, bytes: Vec<u8>) {
        if self
            .sender
            .try_send(Frame::Authoritative { generation, bytes })
            .is_err()
        {
            self.lose(generation);
        }
    }

    /// Loss of any lifecycle frame invalidates authoritative observation too.
    /// Never wait for the sink, queue capacity, or a reset acknowledgement.
    pub(crate) fn publish_event(&self, event: &RuntimeEvent) {
        let Ok(_claim) = self.gate.active.try_read() else {
            // There is no generation token yet. A rotation may be in progress;
            // record loss separately so it cannot disappear inside its commit.
            self.gate.lost.store(true, Ordering::Release);
            return;
        };
        let Some(generation) = self.generation() else {
            return;
        };
        match encode_scoped(&self.source, generation, event) {
            Ok(frame) => self.enqueue_authoritative(generation, frame),
            Err(_) => self.lose(generation),
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
    diagnostics_lost: &AtomicBool,
    admission: &AtomicU64,
    runtime_events: bool,
    source: &str,
    gate: &PublicationGate,
) -> io::Result<()> {
    let mut heartbeat = Instant::now();
    let mut opened = heartbeat;
    // The initial boundary always names epoch zero, even if a producer lost it
    // before the worker started. Loss and its replacement follow in wire order.
    transport_status(&mut writer, runtime_events, source, 0, BoundaryState::Open)?;
    loop {
        if gate.lost.swap(false, Ordering::AcqRel) {
            admission.fetch_or(1, Ordering::AcqRel);
        }
        let generation = admission.load(Ordering::Acquire);
        // Healthy periodic boundaries need not discard any observations. A
        // successful exclusive claim proves all producer claims have completed;
        // drain their bounded queue BEFORE changing epochs. Failed claims record
        // loss separately (above), including attempts during the boundary write.
        let rotation = if generation.is_multiple_of(2) && opened.elapsed() >= ROTATE {
            gate.active.try_write().ok()
        } else {
            None
        };
        if rotation.is_some() {
            for frame in receiver.try_iter().take(CAPACITY) {
                match frame {
                    Frame::Authoritative {
                        generation: token,
                        bytes,
                    } if token == admission.load(Ordering::Acquire) => writer.write_all(&bytes)?,
                    Frame::Diagnostic(bytes) => writer.write_all(&bytes)?,
                    _ => {}
                }
            }
            // Authoritative publication is excluded; diagnostics may continue.
            // The production queue is bounded at CAPACITY, so this drain is
            // finite even when arbitrary diagnostic producers keep publishing.
            if gate.lost.swap(false, Ordering::AcqRel) {
                admission.fetch_or(1, Ordering::AcqRel);
            }
        }
        let generation = admission.load(Ordering::Acquire);
        if !generation.is_multiple_of(2) || rotation.is_some() {
            let closed = generation | 1;
            if generation.is_multiple_of(2) {
                let _ = admission.compare_exchange(
                    generation,
                    closed,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            } else {
                transport_status(
                    &mut writer,
                    runtime_events,
                    source,
                    generation - 1,
                    BoundaryState::Lost,
                )?;
            }
            if closed == u64::MAX {
                return transport_status(
                    &mut writer,
                    runtime_events,
                    source,
                    closed - 1,
                    BoundaryState::Lost,
                );
            }
            transport_status(
                &mut writer,
                runtime_events,
                source,
                closed + 1,
                BoundaryState::Open,
            )?;
            let _ =
                admission.compare_exchange(closed, closed + 1, Ordering::AcqRel, Ordering::Acquire);
            heartbeat = Instant::now();
            opened = heartbeat;
        }
        // IO above is an external callback under this unit guard. Publication
        // reentry only try_reads, so it records loss rather than deadlocking.
        // A Write panic poisons the gate and WorkerGuard retires the worker;
        // no poison recovery is attempted. Release before receiving more work.
        drop(rotation);
        if diagnostics_lost.swap(false, Ordering::AcqRel) {
            writer.write_all(b"kit: some child diagnostics were dropped\n")?;
        }
        match receiver.recv_timeout(HEARTBEAT) {
            Ok(frame) => {
                let bytes = match frame {
                    Frame::Authoritative { generation, .. }
                        if generation != admission.load(Ordering::Acquire) =>
                    {
                        continue;
                    }
                    Frame::Authoritative { bytes, .. } | Frame::Diagnostic(bytes) => bytes,
                };
                // A loss concurrent with this write is ordered AFTER it. The
                // single writer cannot let any selected old frame cross reset.
                writer.write_all(&bytes)?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return transport_status(
                    &mut writer,
                    runtime_events,
                    source,
                    admission.load(Ordering::Acquire) & !1,
                    BoundaryState::Lost,
                );
            }
        }
        let generation = admission.load(Ordering::Acquire);
        if generation.is_multiple_of(2) && heartbeat.elapsed() >= HEARTBEAT {
            transport_status(
                &mut writer,
                runtime_events,
                source,
                generation,
                BoundaryState::Heartbeat,
            )?;
            heartbeat = Instant::now();
        }
    }
}

fn transport_status(
    writer: &mut impl Write,
    enabled: bool,
    source: &str,
    epoch: u64,
    state: BoundaryState,
) -> io::Result<()> {
    if enabled {
        write_frame(
            writer,
            &RuntimeEvent::RuntimeBoundary {
                source: source.into(),
                epoch,
                state,
            },
        )?;
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct ScopedFrame<'a> {
    event: &'static str,
    source: &'a str,
    epoch: u64,
    payload: &'a RuntimeEvent,
}

fn encode_scoped(source: &str, epoch: u64, event: &RuntimeEvent) -> io::Result<Vec<u8>> {
    let mut inner = event;
    let mut depth = 1; // Include the scope this publisher adds.
    while let RuntimeEvent::RuntimeScoped { payload, .. } = inner {
        depth += 1;
        if depth > 16 {
            return Err(io::Error::other("runtime provenance is too deeply nested"));
        }
        inner = payload;
    }
    if !event.valid_wire_payload() {
        return Err(io::Error::other("invalid runtime metadata"));
    }
    encode_value(&ScopedFrame {
        event: "runtime_scoped",
        source,
        epoch,
        payload: event,
    })
}

fn write_frame(writer: &mut impl Write, event: &RuntimeEvent) -> io::Result<()> {
    // One whole-frame stderr lock, owned only by the detached worker.
    writer.write_all(&encode_frame(event)?)
}

fn encode_frame(event: &RuntimeEvent) -> io::Result<Vec<u8>> {
    if !event.valid_wire_payload() {
        return Err(io::Error::other("invalid runtime metadata"));
    }
    encode_value(event)
}

fn encode_value(event: &impl serde::Serialize) -> io::Result<Vec<u8>> {
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
