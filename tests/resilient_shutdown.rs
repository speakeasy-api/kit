//! Keep the process-global exhaustion test separate from ordinary sessions.
use std::{
    io,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use kit::resilient_fs::{self as fs, Fs};
#[path = "support/capacity.rs"]
mod capacity;
use capacity::{Capacity, CapacityDisk};

#[tokio::test]
async fn exhausted_storage_requests_orderly_process_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let capacity = Arc::new(Capacity {
        exhausted: AtomicBool::new(true),
        exhaust_on_write: AtomicBool::new(false),
        repaired: directory.path().join("repair"),
    });
    fs::initialize_global(Fs::with_budget(Arc::new(CapacityDisk(capacity)), 0, 0)).unwrap();
    fs::start_recovery_worker();
    let path = directory.path().join("cannot-retain");
    let error = fs::write(&path, b"too much for this budget").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::OutOfMemory);
    assert!(fs::global().status().exhausted);
    tokio::time::timeout(Duration::from_secs(5), fs::shutdown_token().cancelled())
        .await
        .expect("exhaustion must cancel the process, not panic or hang");
    assert!(
        !path.exists(),
        "a rejected write must not publish partial data"
    );
}
