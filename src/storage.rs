//! Bounded volatile storage for internal persistence failures.
//!
//! Capacity errors are distinct from permission, locking and integrity errors:
//! only the former may relax durability. This is not a virtual filesystem and
//! must not be used to claim a user-requested file edit succeeded.

use std::{
    io::{self, Write},
    sync::atomic::{AtomicUsize, Ordering},
};

const MAX_FALLBACK_BYTES: usize = 64 * 1024 * 1024;
static RESERVED_BYTES: Budget = Budget(AtomicUsize::new(0));

pub(crate) fn is_capacity_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

struct Budget(AtomicUsize);

impl Budget {
    fn reserve(&self, additional: usize, limit: usize) -> io::Result<()> {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(additional).filter(|next| *next <= limit)
            })
            .map(|_| ())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn release(&self, bytes: usize) {
        self.0.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// An append-only buffer sharing a process-wide allocation budget.
///
/// Growth is fallible, including during serde serialization through `Write`.
/// Dropping a buffer releases its reservation. No disk retry occurs implicitly:
/// callers must never append beyond a missing durable record.
#[derive(Default)]
pub(crate) struct MemoryBuffer {
    bytes: Vec<u8>,
    reserved: usize,
}

impl MemoryBuffer {
    /// Exit before a serializer can heap-allocate an error wrapper on OOM.
    pub(crate) fn writer_or_exit(&mut self) -> impl Write + '_ {
        ExitOnFailure(self)
    }

    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    fn reserve(&mut self, needed: usize) -> io::Result<()> {
        if needed > self.reserved {
            // Geometric growth avoids reallocating for every serializer token.
            let target = needed
                .checked_next_power_of_two()
                .unwrap_or(needed)
                .min(MAX_FALLBACK_BYTES)
                .max(needed);
            let additional = target - self.reserved;
            RESERVED_BYTES.reserve(additional, MAX_FALLBACK_BYTES)?;
            if self
                .bytes
                .try_reserve_exact(target - self.bytes.len())
                .is_err()
            {
                RESERVED_BYTES.release(additional);
                return Err(io::ErrorKind::OutOfMemory.into());
            }
            self.reserved = target;
        }
        Ok(())
    }
}

impl Write for MemoryBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let needed = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        self.reserve(needed)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ExitOnFailure<'a>(&'a mut MemoryBuffer);

impl Write for ExitOnFailure<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.0.write(bytes) {
            Ok(written) => Ok(written),
            Err(_) => exit_exhausted(),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MemoryBuffer {
    fn drop(&mut self) {
        // Free the allocation before another thread can reuse its budget.
        drop(std::mem::take(&mut self.bytes));
        RESERVED_BYTES.release(self.reserved);
    }
}

/// The observer API cannot return a persistence error to its caller. Stop
/// without unwinding or allocating an error report when volatile storage fills.
/// This cannot recover arbitrary allocator aborts elsewhere in the process.
pub(crate) fn exit_exhausted() -> ! {
    crate::tui::restore_after_storage_failure();
    let _ = io::stderr().write_all(
        b"kit: disk persistence failed and the memory fallback is exhausted; exiting. Unsaved session records will be lost.\n",
    );
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_capacity_errors_allow_fallback() {
        for kind in [io::ErrorKind::StorageFull, io::ErrorKind::QuotaExceeded] {
            assert!(is_capacity_error(&kind.into()));
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::NotFound,
            io::ErrorKind::WriteZero,
            io::ErrorKind::Other,
        ] {
            assert!(!is_capacity_error(&kind.into()));
        }
    }

    #[test]
    fn budget_rejects_overflow_and_releases_reservations() {
        let budget = Budget(AtomicUsize::new(0));
        budget.reserve(8, 10).unwrap();
        assert!(budget.reserve(3, 10).is_err());
        assert!(budget.reserve(usize::MAX, usize::MAX).is_err());
        assert_eq!(budget.0.load(Ordering::Relaxed), 8);
        budget.release(8);
        budget.reserve(10, 10).unwrap();
    }

    #[test]
    fn buffer_writes_and_failed_growth_preserves_content() {
        let mut buffer = MemoryBuffer::default();
        buffer.write_all(b"hello").unwrap();
        buffer.write_all(b" world").unwrap();
        assert_eq!(buffer.as_slice(), b"hello world");
        // Exercise a budget refusal without allocating a huge input or relying
        // on other concurrently running tests' reservations.
        let reserved = buffer.reserved;
        assert_eq!(
            buffer.reserve(MAX_FALLBACK_BYTES + 1).unwrap_err().kind(),
            io::ErrorKind::OutOfMemory
        );
        assert_eq!(buffer.reserved, reserved);
        assert_eq!(buffer.as_slice(), b"hello world");
    }

    #[test]
    fn exhaustion_exits_without_a_panic() {
        const CHILD_FLAG: &str = "KIT_TEST_STORAGE_EXHAUSTION_CHILD";
        if std::env::var_os(CHILD_FLAG).is_some() {
            // Reserve the budget without actually filling memory. Exercise the
            // serialization adapter's exit, not just the exit helper itself.
            RESERVED_BYTES
                .reserve(MAX_FALLBACK_BYTES, MAX_FALLBACK_BYTES)
                .unwrap();
            let mut buffer = MemoryBuffer::default();
            let _ = serde_json::to_writer(buffer.writer_or_exit(), &"no budget left");
            unreachable!("the exhausted writer must exit");
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::tests::exhaustion_exits_without_a_panic",
                "--nocapture",
            ])
            .env(CHILD_FLAG, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("memory fallback is exhausted"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}
