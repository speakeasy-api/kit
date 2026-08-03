#![cfg(debug_assertions)]

use std::{fs, path::Path, process::Command};

mod agent_run;
mod backup_restore;
mod cli_daemon;
mod daemon_lifecycle;
mod daemon_telemetry;
mod local_executor;
mod provider_stream;

fn run(root: &Path, program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"))
}

fn diagnostic(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_release_pending_fails(root: &Path) {
    let fixture = "tests/conformance/req_lint_cases/pending_evidence";
    let release = run(
        root,
        "python3",
        &[
            "scripts/req_lint.py",
            "--registry-dir",
            fixture,
            "--rfc",
            "tests/conformance/req_lint_cases/pending_evidence/RFC.md",
            "--scan-dir",
            fixture,
            "--aggregate",
            "--release-candidate",
        ],
    );
    let release_diagnostic = diagnostic(&release);
    assert!(!release.status.success(), "pending release evidence passed");
    assert!(release_diagnostic.contains("missing-stale-or-failing-evidence:"));
}

#[test]
fn release_candidate_rejects_pending_evidence() {
    assert_release_pending_fails(Path::new(env!("CARGO_MANIFEST_DIR")));
}

#[test]
fn phase0_governance_contracts_hold_and_release_pending_fails() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_release_pending_fails(root);

    let contracts = [
        ("python3", vec!["scripts/generate_registry.py", "--check"]),
        ("python3", vec!["scripts/req_lint.py", "--aggregate"]),
        ("sh", vec!["scripts/verify_pins.sh"]),
        ("sh", vec!["scripts/lint_threat_model.sh"]),
        ("python3", vec!["scripts/dashboard_lint.py"]),
        ("python3", vec!["scripts/lane_lint.py"]),
    ];

    for (program, args) in contracts {
        let output = run(root, program, &args);
        assert!(
            output.status.success(),
            "{program} {} failed\n{}",
            args.join(" "),
            diagnostic(&output)
        );
    }
}

#[test]
fn phase0_g00_attestations_report_current_or_deliberately_pending() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let current = run(root, "python3", &["scripts/validate_g00_attestations.py"]);
    let current_diagnostic = diagnostic(&current);
    assert!(
        !current.status.success(),
        "G00 evidence must remain pending"
    );
    assert!(
        current_diagnostic.contains("passing evidence set differs from G00 commands")
            && current_diagnostic.contains("EV-1.09-C-001")
            && current_diagnostic.contains("EV-1.09-C-002")
            && current_diagnostic.contains("EV-1.09-C-003")
            && current_diagnostic.contains("EV-1.09-C-004"),
        "unexpected pending G00 diagnostic\n{current_diagnostic}"
    );

    let records: Vec<serde_yaml::Value> = serde_yaml::from_slice(
        &fs::read(root.join("requirements/registry.d/KIT-VERSION.yaml")).unwrap(),
    )
    .unwrap();
    for id in [
        "KIT-VERSION-001",
        "KIT-VERSION-002",
        "KIT-VERSION-003",
        "KIT-VERSION-004",
    ] {
        let record = records.iter().find(|record| record["id"] == id).unwrap();
        assert_eq!(record["latest_result"], "pending");
        for field in ["artifact_digest", "environment_digest", "versions"] {
            assert!(record[field].is_null(), "{id}.{field} must remain null");
        }
    }
}

#[test]
fn threat_model_linter_rejects_nonexistent_implemented_reference() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let seed_dir =
        std::env::temp_dir().join(format!("kit-threat-model-lint-{}", std::process::id()));
    fs::create_dir_all(&seed_dir).expect("create threat-model lint seed directory");
    let seeded_matrix = seed_dir.join("fault-matrix.md");
    let matrix = fs::read_to_string(root.join("docs/decisions/fault-matrix.md"))
        .expect("read fault matrix")
        .replace(
            "tests/integration/main.rs::release_candidate_rejects_pending_evidence",
            "tests/integration/main.rs::nonexistent_implemented_reference",
        );
    fs::write(&seeded_matrix, matrix).expect("write seeded fault matrix");

    let output = Command::new("sh")
        .arg("scripts/lint_threat_model.sh")
        .arg("docs/decisions/threat-model.md")
        .arg(&seeded_matrix)
        .current_dir(root)
        .output()
        .expect("run threat-model linter against seeded matrix");
    fs::remove_dir_all(&seed_dir).expect("remove threat-model lint seed directory");

    assert!(
        !output.status.success(),
        "nonexistent evidence reference passed"
    );
    assert!(
        diagnostic(&output)
            .contains("implemented test symbol does not exist as an exact #[test] fn")
    );
}
