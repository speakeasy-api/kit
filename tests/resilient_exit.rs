// Integration crate and its helpers are test-only. Placeholders stay denied.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

//! Final process-exit recovery must not depend on the background worker waking.
#[path = "support/capacity.rs"]
mod capacity;

use capacity::{Capacity, CapacityDisk};
use kit::resilient_fs::{DiskBackend, Fs, finish_recovery};
use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn pending_replacement(optional: bool) -> (tempfile::TempDir, PathBuf, Arc<Capacity>, Fs) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transcript.jsonl");
    fs::write(&path, b"old generation\n").unwrap();
    let backend = Arc::new(Capacity {
        exhausted: AtomicBool::new(true),
        exhaust_on_write: AtomicBool::new(false),
        repaired: directory.path().join("capacity-repaired"),
    });
    let filesystem = Fs::new(Arc::new(CapacityDisk(backend.clone())));
    let filesystem = if optional {
        filesystem.best_effort(1024, 16)
    } else {
        filesystem
    };
    filesystem.replace(&path, b"accepted generation\n").unwrap();
    let status = filesystem.status();
    assert!(status.pending_operations > 0);
    assert!(!status.exhausted, "ordinary degradation is below budget");
    assert_eq!(fs::read(&path).unwrap(), b"old generation\n");
    (directory, path, backend, filesystem)
}

#[test]
fn final_pass_persists_below_budget_changes_after_capacity_returns() {
    let (_directory, path, backend, filesystem) = pending_replacement(false);
    backend.exhausted.store(false, Ordering::SeqCst);
    // No facade reads or worker runs between repairing capacity and finalization.
    finish_recovery(&filesystem).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"accepted generation\n");
    assert_eq!(filesystem.status().pending_operations, 0);
    assert!(kit::resilient_fs::shutdown_token().is_cancelled());
}

#[test]
fn final_pass_reports_undurable_changes_even_below_budget() {
    let (_directory, path, _backend, filesystem) = pending_replacement(false);
    let error = finish_recovery(&filesystem).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    assert!(
        error
            .to_string()
            .contains("could not be persisted before exit")
    );
    assert!(filesystem.status().pending_operations > 0);
    assert!(!filesystem.status().exhausted);
    assert_eq!(fs::read(path).unwrap(), b"old generation\n");
}

#[test]
fn optional_final_pass_allows_loss_and_retries_if_capacity_returns() {
    use kit::resilient_fs::{BestEffortStatus, finish_best_effort_recovery};
    let (_directory, path, backend, filesystem) = pending_replacement(true);
    // The infallible final pass leaves the bounded buffer available for a later
    // retry, but does not claim the on-disk copy is current or fail the command.
    finish_best_effort_recovery(&filesystem);
    assert_eq!(
        filesystem.best_effort_status(),
        Some(BestEffortStatus::Buffered)
    );
    assert_eq!(filesystem.read(&path).unwrap(), b"accepted generation\n");
    assert_eq!(fs::read(&path).unwrap(), b"old generation\n");
    backend.exhausted.store(false, Ordering::SeqCst);
    finish_best_effort_recovery(&filesystem);
    assert_eq!(
        filesystem.best_effort_status(),
        Some(BestEffortStatus::Ready)
    );
    assert_eq!(fs::read(&path).unwrap(), b"accepted generation\n");
}

#[test]
fn optional_final_pass_discards_an_exhausted_domain_without_tail_replay() {
    use kit::resilient_fs::{BestEffortStatus, finish_best_effort_recovery};
    let (_directory, path, backend, filesystem) = pending_replacement(true);
    assert!(filesystem.replace(&path, &[b'x'; 4096]).is_err());
    assert_eq!(
        filesystem.best_effort_status(),
        Some(BestEffortStatus::Dropped)
    );
    finish_best_effort_recovery(&filesystem);
    assert_eq!(filesystem.status().pending_operations, 0);
    assert_eq!(filesystem.status().retained_bytes, 0);
    assert!(!filesystem.status().exhausted);
    backend.exhausted.store(false, Ordering::SeqCst);
    finish_best_effort_recovery(&filesystem);
    assert!(filesystem.replace(&path, b"later tail").is_err());
    assert_eq!(fs::read(path).unwrap(), b"old generation\n");
}

#[test]
fn final_pass_accepts_an_already_durable_service() {
    let filesystem = Fs::new(Arc::new(DiskBackend));
    finish_recovery(&filesystem).unwrap();
    assert_eq!(filesystem.status().pending_operations, 0);
}
