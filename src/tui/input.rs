//! Single-owner terminal input. Create only after capability queries finish;
//! drop (and join) before restoring the terminal or handing stdin to a child.
//! Image/keyboard capability queries may read synchronously during setup, before
//! this owner exists. During its lifetime the UI only consumes the bounded queue:
//! it never polls the OS input reader or acquires crossterm's input lock.
use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::event::{self, Event};
use futures_util::Stream;
use tokio::sync::mpsc;

pub(super) struct Events {
    receiver: mpsc::Receiver<io::Result<Event>>,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Events {
    pub(super) fn new() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel(256);
        let stopped = Arc::new(AtomicBool::new(false));
        let reader_stopped = Arc::clone(&stopped);
        let reader = thread::Builder::new()
            .name("kit-terminal-input".into())
            .spawn(move || {
                // No other EventStream or synchronous reader may coexist with this owner.
                // poll and read always execute on this same OS thread.
                while !reader_stopped.load(Ordering::Acquire) {
                    let event = match event::poll(Duration::from_millis(25)) {
                        Ok(false) => continue,
                        Ok(true) => {
                            if reader_stopped.load(Ordering::Acquire) {
                                break;
                            }
                            event::read()
                        }
                        Err(error) => Err(error),
                    };
                    let failed = event.is_err();
                    if reader_stopped.load(Ordering::Acquire)
                        || sender.blocking_send(event).is_err()
                        || failed
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            receiver,
            stopped,
            reader: Some(reader),
        })
    }

    pub(super) fn try_next(&mut self) -> Option<io::Result<Event>> {
        self.receiver.try_recv().ok()
    }
}

impl Stream for Events {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(cx)
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        // Drop is the sole stop writer. Closing first wakes a sender blocked on
        // bounded-channel capacity; the flag bounds an idle reader's next poll.
        // Joining hands exclusive terminal ownership back to auth/teardown.
        self.receiver.close();
        self.stopped.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            // A worker panic is already reported by the panic hook and closes
            // the stream. Never turn cleanup (possibly unwinding) into a panic.
            let _ = reader.join();
        }
    }
}

#[cfg(all(test, unix))]
#[path = "input_tests.rs"]
mod tests;
