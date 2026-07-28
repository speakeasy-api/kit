use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use kit::{
    domain::secret::SecretLease,
    store::artifacts::{ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore},
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
    verify::feedback::{
        BaselineAvailability, BaselineUnavailableReason, DiagnosticAdapter, DiagnosticDeltaKind,
        DiagnosticReport, DiagnosticSet, FeedbackItem, FeedbackLimits, OpaqueArtifactRef,
        RequiredFailure, compare_diagnostics, parse_diagnostics, render_feedback,
    },
};

fn diagnostic(path: &str, line: u32, code: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "path": path,
        "range": {
            "start_line": line,
            "start_column": 1,
            "end_line": line,
            "end_column": 5
        },
        "code": code,
        "message": message,
        "severity": "error",
        "tool": "rustc"
    }))
    .unwrap()
}

fn set(revision: &str, records: &[Vec<u8>]) -> DiagnosticSet {
    let bytes = records
        .iter()
        .flat_map(|record| record.iter().copied().chain([b'\n']))
        .collect::<Vec<_>>();
    let mut set = parse_diagnostics(
        DiagnosticAdapter::NormalizedJsonLinesV1,
        "cargo-check",
        "rustc",
        &bytes,
        &FeedbackLimits::default(),
    )
    .unwrap();
    set.revision = revision.to_owned();
    set
}

fn opaque(seed: u8) -> OpaqueArtifactRef {
    OpaqueArtifactRef {
        reference: format!("artifact-ref:{}", format!("{seed:02x}").repeat(32)),
        length: 123,
    }
}

#[test]
fn red_baseline_delta_is_complete_and_moved_lines_are_attributed() {
    let before = set(
        "revision-before",
        &[
            diagnostic("src/new.rs", 12, "E1", "resolved"),
            diagnostic("src/new.rs", 13, "E2", "same"),
            diagnostic("src/new.rs", 14, "E3", "old message"),
        ],
    );
    let after = set(
        "revision-after",
        &[
            diagnostic("src/new.rs", 13, "E2", "same"),
            diagnostic("src/new.rs", 14, "E3", "new message"),
            diagnostic("src/new.rs", 15, "E4", "new"),
        ],
    );
    let compared = compare_diagnostics(Some(&before), &after).unwrap();
    let kinds = compared
        .deltas
        .iter()
        .map(|delta| delta.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            DiagnosticDeltaKind::New,
            DiagnosticDeltaKind::Changed,
            DiagnosticDeltaKind::Persisting,
            DiagnosticDeltaKind::Resolved,
        ]
    );
}

#[test]
fn unavailable_or_incompatible_baseline_never_claims_a_new_edit_failure() {
    let before = set(
        "revision-before",
        &[diagnostic("src/lib.rs", 1, "E1", "existing")],
    );
    let mut after = set(
        "revision-after",
        &[diagnostic("src/lib.rs", 1, "E1", "existing")],
    );
    let absent = compare_diagnostics(None, &after).unwrap();
    assert_eq!(
        absent.baseline,
        BaselineAvailability::Unavailable(BaselineUnavailableReason::Missing)
    );
    assert_eq!(absent.deltas[0].kind, DiagnosticDeltaKind::Observed);

    after.checks[0].tool_digest = Some(format!("blake3:{}", "a".repeat(64)));
    let mismatch = compare_diagnostics(Some(&before), &after).unwrap();
    assert_eq!(
        mismatch.baseline,
        BaselineAvailability::Unavailable(BaselineUnavailableReason::ToolVersionMismatch)
    );
    assert_eq!(mismatch.deltas[0].kind, DiagnosticDeltaKind::Observed);
}

#[test]
fn hostile_logs_are_bounded_and_malformed_records_are_counted() {
    let limits = FeedbackLimits {
        max_input_bytes: 1024,
        max_record_bytes: 64,
        max_diagnostics: 2,
        max_operation_time: Duration::from_secs(1),
        ..FeedbackLimits::default()
    };
    let mut input = vec![b'x'; 65];
    input.extend_from_slice(b"\n{not-json}\n");
    let parsed = parse_diagnostics(
        DiagnosticAdapter::NormalizedJsonLinesV1,
        "check",
        "rustc",
        &input,
        &limits,
    )
    .unwrap();
    assert_eq!(parsed.oversized_records, 1);
    assert_eq!(parsed.malformed_records, 1);
    assert!(parsed.diagnostics.is_empty());

    assert!(
        parse_diagnostics(
            DiagnosticAdapter::NormalizedJsonLinesV1,
            "check",
            "rustc",
            &vec![0; 1025],
            &limits,
        )
        .is_err()
    );
}

#[test]
fn bounded_feedback_keeps_required_failure_and_full_canonical_report() {
    let current = set(
        "revision-after",
        &(0..40)
            .map(|line| diagnostic("src/lib.rs", line + 1, "E1", &format!("failure {line}")))
            .collect::<Vec<_>>(),
    );
    let comparison = compare_diagnostics(None, &current).unwrap();
    let report = DiagnosticReport {
        schema_version: 1,
        baseline: comparison.baseline.clone(),
        baseline_set: None,
        current_set: current.clone(),
        deltas: comparison.deltas.clone(),
    };
    let root = std::env::temp_dir().join(format!(
        "kit-feedback-report-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&root);
    let artifacts = ArtifactStore::open(&root).unwrap();
    let report_bytes = serde_json::to_vec(&report).unwrap();
    let artifact = artifacts
        .put(
            &report_bytes,
            ArtifactMetadata::new(
                "application/json",
                ArtifactClass::Report,
                "principal",
                "project",
                ArtifactRetention::Forever,
                1,
            )
            .unwrap(),
        )
        .unwrap();
    let payload = render_feedback(
        &comparison,
        0,
        current.diagnostics.len(),
        current.sanitized_input_bytes,
        &[RequiredFailure {
            check_id: "cargo-check".into(),
            status: "nonzero".into(),
            exit_code: Some(1),
        }],
        OpaqueArtifactRef {
            reference: artifact.reference().to_string(),
            length: artifact.manifest().size,
        },
        vec![opaque(2), opaque(3)],
        2048,
    )
    .unwrap();
    assert!(payload.truncated);
    assert_eq!(
        payload.canonical_bytes().len() as u64,
        payload.counts.serialized_bytes
    );
    assert!(payload.canonical_bytes().len() <= 2048);
    assert!(matches!(
        payload.items.first(),
        Some(FeedbackItem::RequiredFailure(_))
    ));
    let stored = artifacts
        .open_bytes_bounded(artifact.digest(), report_bytes.len())
        .unwrap();
    let retained: DiagnosticReport = serde_json::from_slice(&stored).unwrap();
    assert_eq!(retained.current_set.diagnostics.len(), 40);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn secret_canaries_in_plain_url_base64_split_and_binary_forms_never_reach_feedback() {
    let binary = vec![0, 255, 1, 2, 3, 128];
    let leases = [
        SecretLease::new(b"top+/secret".to_vec()),
        SecretLease::new(binary.clone()),
    ];
    let redactor = CaptureRedactor::new(&leases);
    let mut capture = redactor.start(CaptureBoundary::Artifact);
    for chunk in [
        b"plain top+/".as_slice(),
        b"secret url top%2B%2Fsecret b64 dG9wKy9zZWNyZXQ= binary ".as_slice(),
        binary[..3].as_ref(),
        binary[3..].as_ref(),
    ] {
        capture.push(chunk).unwrap();
    }
    capture.finish().unwrap();
    let bytes = capture.bytes().unwrap();
    assert!(
        !bytes
            .windows(b"top+/secret".len())
            .any(|value| value == b"top+/secret")
    );
    assert!(
        !bytes
            .windows(b"top%2B%2Fsecret".len())
            .any(|value| value == b"top%2B%2Fsecret")
    );
    assert!(
        !bytes
            .windows(b"dG9wKy9zZWNyZXQ=".len())
            .any(|value| value == b"dG9wKy9zZWNyZXQ=")
    );
    assert!(!bytes.windows(binary.len()).any(|value| value == binary));
}

#[test]
fn downstream_cannot_mint_feedback_authority_or_append_events() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let deps = manifest.join("target/debug/deps");
    let library = newest_rlib(&deps).expect("kit rlib");
    let root = std::env::temp_dir().join(format!(
        "kit-feedback-authority-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::create_dir_all(&root).unwrap();
    let source = root.join("forge.rs");
    fs::write(
        &source,
        r#"
use kit::verify::feedback::{CheckEventKind, FeedbackAuthority, FeedbackEventStore};
use kit::store::artifacts::ArtifactStore;
fn main() {
    let _ = FeedbackAuthority::issue("run", "edit", 1);
    let _ = FeedbackEventStore::append;
    let _ = CheckEventKind::Started;
    let _ = ArtifactStore::open_reference;
}
"#,
    )
    .unwrap();
    let output = Command::new("rustc")
        .arg("--edition=2024")
        .arg(&source)
        .arg("--extern")
        .arg(format!("kit={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("--out-dir")
        .arg(&root)
        .output()
        .unwrap();
    fs::remove_dir_all(&root).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.matches("private").count() >= 3, "{stderr}");
}

fn newest_rlib(directory: &Path) -> Option<PathBuf> {
    let mut libraries = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libkit-") && name.ends_with(".rlib"))
        })
        .collect::<Vec<_>>();
    libraries.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    libraries.pop()
}
