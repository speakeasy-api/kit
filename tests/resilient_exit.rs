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

fn pending_replacement() -> (tempfile::TempDir, PathBuf, Arc<Capacity>, Fs) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transcript.jsonl");
    fs::write(&path, b"old generation\n").unwrap();
    let backend = Arc::new(Capacity {
        exhausted: AtomicBool::new(true),
        repaired: directory.path().join("capacity-repaired"),
    });
    let filesystem = Fs::new(Arc::new(CapacityDisk(backend.clone())));
    filesystem.replace(&path, b"accepted generation\n").unwrap();
    let status = filesystem.status();
    assert!(status.pending_operations > 0);
    assert!(!status.exhausted, "ordinary degradation is below budget");
    assert_eq!(fs::read(&path).unwrap(), b"old generation\n");
    (directory, path, backend, filesystem)
}

#[test]
fn final_pass_persists_below_budget_changes_after_capacity_returns() {
    let (_directory, path, backend, filesystem) = pending_replacement();
    backend.exhausted.store(false, Ordering::SeqCst);
    // No facade reads or worker runs between repairing capacity and finalization.
    finish_recovery(&filesystem).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"accepted generation\n");
    assert_eq!(filesystem.status().pending_operations, 0);
    assert!(kit::resilient_fs::shutdown_token().is_cancelled());
}

#[test]
fn final_pass_reports_undurable_changes_even_below_budget() {
    let (_directory, path, _backend, filesystem) = pending_replacement();
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
fn final_pass_accepts_an_already_durable_service() {
    let filesystem = Fs::new(Arc::new(DiskBackend));
    finish_recovery(&filesystem).unwrap();
    assert_eq!(filesystem.status().pending_operations, 0);
}
