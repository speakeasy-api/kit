use std::{
    io::{self, Cursor, Read},
    sync::{Arc, Barrier},
};

use kit::executor::process::{
    own::{TRUNCATION_MARKER, capture_bounded},
    tree::HostSupport,
};
use kit::telemetry::redact::{CaptureBoundary, CaptureRedactor};

fn bytes(stream: &kit::executor::process::own::CapturedStream) -> Vec<u8> {
    stream
        .sanitize(&CaptureRedactor::new(&[]), CaptureBoundary::Log)
        .bytes()
        .unwrap()
        .to_vec()
}

#[test]
fn excess_output_is_always_bounded_marked_and_accounted() {
    const BOUND: u64 = 128;
    let stdout = ["utf8: snowman ", "\u{2603}\nline boundary\n"]
        .concat()
        .repeat(12);
    let mut stderr = (0_u8..=255).collect::<Vec<_>>();
    stderr.extend_from_slice(b"\nnot a text line\n");

    for _ in 0..100 {
        let output = capture_bounded(
            Cursor::new(stdout.as_bytes().to_vec()),
            Cursor::new(stderr.clone()),
            BOUND,
        )
        .unwrap();

        assert!(output.retained_bytes() <= BOUND as usize);
        assert_eq!(
            output.original_bytes(),
            (stdout.len() + stderr.len()) as u64
        );
        assert_eq!(
            output.truncated_bytes(),
            output.original_bytes()
                - (output.retained_bytes() - 2 * TRUNCATION_MARKER.len()) as u64
        );
        let retained_per_stream = BOUND as usize / 2 - TRUNCATION_MARKER.len();
        assert_eq!(
            &bytes(&output.stdout)[..retained_per_stream],
            &stdout.as_bytes()[..retained_per_stream]
        );
        assert_eq!(
            &bytes(&output.stderr)[..retained_per_stream],
            &stderr[..retained_per_stream]
        );
        for stream in [&output.stdout, &output.stderr] {
            let bytes = bytes(stream);
            assert!(stream.was_truncated());
            assert!(bytes.ends_with(TRUNCATION_MARKER));
            assert_eq!(
                stream.original_bytes(),
                (bytes.len() - TRUNCATION_MARKER.len()) as u64 + stream.truncated_bytes()
            );
        }
    }
}

#[test]
fn stdout_and_stderr_are_drained_concurrently() {
    let barrier = Arc::new(Barrier::new(2));
    let stdout = ConcurrentReader::new(barrier.clone(), vec![b'o'; 4096]);
    let stderr = ConcurrentReader::new(barrier, vec![b'e'; 4096]);

    let output = capture_bounded(stdout, stderr, 256).unwrap();

    assert_eq!(output.original_bytes(), 8192);
    assert_eq!(output.retained_bytes(), 256);
    assert!(bytes(&output.stdout).ends_with(TRUNCATION_MARKER));
    assert!(bytes(&output.stderr).ends_with(TRUNCATION_MARKER));
}

#[test]
fn host_execution_is_explicitly_unavailable_without_a_complete_boundary() {
    assert!(matches!(
        HostSupport::trusted_local(true),
        HostSupport::Unavailable { .. } | HostSupport::Delegated { .. }
    ));
}

struct ConcurrentReader {
    barrier: Arc<Barrier>,
    bytes: Cursor<Vec<u8>>,
    synchronized: bool,
}

impl ConcurrentReader {
    fn new(barrier: Arc<Barrier>, bytes: Vec<u8>) -> Self {
        Self {
            barrier,
            bytes: Cursor::new(bytes),
            synchronized: false,
        }
    }
}

impl Read for ConcurrentReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.synchronized {
            self.synchronized = true;
            self.barrier.wait();
        }
        self.bytes.read(buffer)
    }
}
