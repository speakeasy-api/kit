use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

const REASONS: [&str; 6] = [
    "unregistered-normative-text",
    "duplicate-requirement-id",
    "missing-governance-fields",
    "unknown-requirement-citation",
    "tombstone-without-replacement",
    "missing-stale-or-failing-evidence",
];

fn run(root: &Path, args: &[&Path]) -> Output {
    let mut command = Command::new("python3");
    command.arg(root.join("scripts/req_lint.py"));
    for arg in args {
        command.arg(arg);
    }
    command
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| panic!("failed to run req_lint.py: {error}"))
}

fn diagnostic(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn temp_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "kit-req-lint-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn aggregate_fixture(root: &Path, name: &str, release_candidate: bool) -> Output {
    let fixture = root.join("tests/conformance/req_lint_cases").join(name);
    let mut command = Command::new("python3");
    command
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--scan-dir")
        .arg(&fixture)
        .arg("--aggregate");
    if release_candidate {
        command.arg("--release-candidate");
    }
    command
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| panic!("{name}: failed to run req_lint.py: {error}"))
}

#[test]
fn req_lint_real_conformance_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cases = [
        ("unregistered", Some(REASONS[0]), false),
        ("duplicate_id", Some(REASONS[1]), false),
        ("missing_governance", Some(REASONS[2]), false),
        ("unknown_citation", Some(REASONS[3]), false),
        ("dangling_tombstone", Some(REASONS[4]), false),
        ("failing_evidence", Some(REASONS[5]), true),
        ("positive", None, false),
    ];

    for (name, expected_reason, release_candidate) in cases {
        let output = aggregate_fixture(root, name, release_candidate);
        let diagnostic = diagnostic(&output);

        match expected_reason {
            Some(expected) => {
                assert!(
                    !output.status.success(),
                    "{} unexpectedly passed\n{}",
                    name,
                    diagnostic
                );
                for reason in REASONS {
                    let emitted = diagnostic
                        .lines()
                        .any(|line| line.starts_with(&format!("{reason}:")));
                    assert_eq!(
                        emitted,
                        reason == expected,
                        "{name}: unexpected named reasons\n{diagnostic}"
                    );
                }
            }
            None => assert!(
                output.status.success(),
                "positive fixture failed\n{}",
                diagnostic
            ),
        }
    }
}

#[test]
fn req_lint_phase0_and_release_candidate_evidence_semantics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let pending = aggregate_fixture(root, "pending_evidence", false);
    assert!(
        pending.status.success(),
        "Phase 0 rejected pending valid evidence\n{}",
        diagnostic(&pending)
    );

    for name in ["pending_evidence", "stale_evidence", "failing_evidence"] {
        let output = aggregate_fixture(root, name, true);
        let diagnostic = diagnostic(&output);
        assert!(
            !output.status.success(),
            "{name} unexpectedly passed RC lint"
        );
        assert!(
            diagnostic.lines().any(|line| line.starts_with(REASONS[5])),
            "{name}: missing evidence rejection\n{diagnostic}"
        );
    }
}

#[test]
fn req_lint_area_na_accepts_only_registered_empty_areas() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/area_na");
    let registry = fixture.join("registry.d");
    let rfc = fixture.join("RFC.md");

    let accepted = run(
        root,
        &[
            Path::new("--registry-dir"),
            &registry,
            Path::new("--rfc"),
            &rfc,
            Path::new("--areas"),
            Path::new("KIT-CONFIG"),
        ],
    );
    assert!(accepted.status.success(), "{}", diagnostic(&accepted));

    let rejected = run(
        root,
        &[
            Path::new("--registry-dir"),
            &registry,
            Path::new("--rfc"),
            &rfc,
            Path::new("--areas"),
            Path::new("KIT-RELEASE"),
        ],
    );
    assert!(!rejected.status.success(), "unregistered empty area passed");
    assert!(diagnostic(&rejected).contains("no-records: KIT-RELEASE"));
}

#[test]
fn req_lint_production_aggregate_ignores_negative_fixture_citations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = run(root, &[Path::new("--aggregate")]);
    assert!(output.status.success(), "{}", diagnostic(&output));
}

#[test]
fn req_lint_default_scan_rejects_unknown_production_citation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/unknown_citation");
    let output = Command::new("python3")
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--aggregate")
        .current_dir(&fixture)
        .output()
        .unwrap_or_else(|error| panic!("failed to run req_lint.py: {error}"));
    let diagnostic = diagnostic(&output);
    assert!(!output.status.success(), "unknown citation passed");
    assert!(
        diagnostic.lines().any(|line| line.starts_with(REASONS[3])),
        "missing citation rejection\n{diagnostic}"
    );
}

#[test]
fn req_lint_rejects_malformed_and_duplicate_key_yaml() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in ["malformed_yaml", "duplicate_yaml"] {
        let output = aggregate_fixture(root, name, false);
        let diagnostic = diagnostic(&output);
        assert!(!output.status.success(), "{name} unexpectedly passed");
        assert!(
            diagnostic.contains("invalid YAML"),
            "{name}: missing parser rejection\n{diagnostic}"
        );
    }
}

#[test]
fn req_lint_rejects_coordinated_rfc_and_registry_identity_reuse() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/coordinated_mutation");
    let output = Command::new("python3")
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--scan-dir")
        .arg(&fixture)
        .arg("--baseline-file")
        .arg(fixture.join("baseline-registry.yaml"))
        .arg("--aggregate")
        .current_dir(root)
        .output()
        .expect("failed to run coordinated mutation fixture");
    let diagnostic = diagnostic(&output);
    assert!(!output.status.success(), "coordinated mutation passed");
    assert!(
        diagnostic.contains("duplicate-requirement-id:")
            && diagnostic.contains("baseline identity changed"),
        "missing historical identity rejection\n{diagnostic}"
    );
}

#[test]
fn req_lint_baseline_ref_cannot_be_shadowed_by_a_working_tree_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/coordinated_mutation");
    let output = Command::new("python3")
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--baseline-ref")
        .arg(fixture.join("baseline-registry.yaml"))
        .arg("--candidate-ref")
        .arg("0000000000000000000000000000000000000000")
        .arg("--aggregate")
        .current_dir(root)
        .output()
        .expect("failed to run baseline shadow fixture");
    let diagnostic = diagnostic(&output);
    assert!(!output.status.success(), "path shadow passed as a Git ref");
    assert!(
        diagnostic.contains("must be an explicit lowercase 40-character commit SHA"),
        "{diagnostic}"
    );
}

#[test]
fn req_lint_release_requires_a_git_baseline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/positive");
    let output = Command::new("python3")
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--aggregate")
        .arg("--release-candidate")
        .current_dir(root)
        .output()
        .unwrap();
    let diagnostic = diagnostic(&output);
    assert!(!output.status.success());
    assert!(
        diagnostic.contains("requires explicit --baseline-ref and --candidate-ref commit SHAs"),
        "{diagnostic}"
    );
}

#[test]
fn req_lint_git_baseline_requires_distinct_ancestor_with_registry() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let revision = |value: &str| {
        let output = Command::new("git")
            .args(["rev-parse", value])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let candidate = revision("HEAD");
    let baseline = revision("HEAD^");
    let fixture = root.join("tests/conformance/req_lint_cases/positive");
    let invoke = |base: &str, candidate: &str| {
        Command::new("python3")
            .arg(root.join("scripts/req_lint.py"))
            .arg("--registry-dir")
            .arg(&fixture)
            .arg("--rfc")
            .arg(fixture.join("RFC.md"))
            .arg("--baseline-ref")
            .arg(base)
            .arg("--candidate-ref")
            .arg(candidate)
            .arg("--aggregate")
            .current_dir(root)
            .output()
            .unwrap()
    };

    let same = invoke(&candidate, &candidate);
    assert!(diagnostic(&same).contains("must be distinct"));
    let missing = invoke(&baseline, &candidate);
    assert!(diagnostic(&missing).contains("has no requirements/registry.yaml"));
}

#[test]
fn req_lint_release_forbids_local_baseline_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/coordinated_mutation");
    let output = Command::new("python3")
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--baseline-file")
        .arg(fixture.join("baseline-registry.yaml"))
        .arg("--aggregate")
        .arg("--release-candidate")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(diagnostic(&output).contains("local testing only"));
}

#[test]
fn req_lint_rejects_mutated_source_revision() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inventory = temp_path("source-revision.yaml");
    let original = fs::read_to_string(root.join("requirements/source-inventory.yaml")).unwrap();
    fs::write(
        &inventory,
        original.replacen(
            "source_revision: f1893b56e9fba01fcf49c64c3cdf65dfdc7c253a",
            "source_revision: 0000000000000000000000000000000000000000",
            1,
        ),
    )
    .unwrap();
    let output = run(
        root,
        &[
            Path::new("--inventory"),
            &inventory,
            Path::new("--aggregate"),
        ],
    );
    fs::remove_file(inventory).ok();
    let diagnostic = diagnostic(&output);
    assert!(!output.status.success());
    assert!(
        diagnostic.contains("source_revision does not resolve"),
        "{diagnostic}"
    );
}

#[test]
fn req_lint_inventory_rejects_ranges_partial_quotes_and_missing_lines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let original = fs::read_to_string(root.join("requirements/source-inventory.yaml")).unwrap();
    let first = original
        .find("- inventory_id: SRC-COVERAGE-0001\n")
        .unwrap();
    let second = original[first + 1..]
        .find("\n- inventory_id: ")
        .map(|offset| first + 1 + offset + 1)
        .unwrap();
    let cases = [
        (
            "range",
            original.replacen(
                "source_anchor: RFC.md:1\n",
                "source_anchor: RFC.md:1-3\n",
                1,
            ),
            "coverage-only atom must cover exactly one source line",
        ),
        (
            "partial-quote",
            original.replacen(
                "source_quote: '# RFC 0001: Kit, an Efficiency-First Coding Agent Runtime'",
                "source_quote: 'RFC 0001: Kit'",
                1,
            ),
            "coverage-only source_quote must equal the complete source line",
        ),
        (
            "missing-line",
            format!("{}{}", &original[..first], &original[second..]),
            "nonblank RFC lines are not inventoried",
        ),
    ];
    for (label, content, expected) in cases {
        let inventory = temp_path(&format!("inventory-{label}.yaml"));
        fs::write(&inventory, content).unwrap();
        let output = run(
            root,
            &[
                Path::new("--inventory"),
                &inventory,
                Path::new("--aggregate"),
            ],
        );
        fs::remove_file(inventory).ok();
        let diagnostic = diagnostic(&output);
        assert!(!output.status.success(), "{label} unexpectedly passed");
        assert!(diagnostic.contains(expected), "{label}: {diagnostic}");
    }
}

#[test]
fn req_lint_rejects_missing_optional_policy_metadata() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let policy = temp_path("optional.yaml");
    let original = fs::read_to_string(root.join("requirements/policy/optional.yaml")).unwrap();
    fs::write(
        &policy,
        original.replacen("  experiment_id: EXP-M005-W07-SCIP-VOI\n", "", 1),
    )
    .unwrap();
    let script = r#"
import sys
sys.path.insert(0, sys.argv[1] + '/scripts')
from req_lint_lib.governance import check_semantics
from req_lint_lib.loader import load_registry_dir
records, _ = load_registry_dir(sys.argv[1] + '/requirements/registry.d')
findings = check_semantics(records, sys.argv[1] + '/RFC.md', optional_policy_path=sys.argv[2])
messages = '\n'.join(f.message for f in findings)
print(messages)
raise SystemExit(0 if 'mechanism fields must be exactly' in messages else 1)
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .arg(&policy)
        .current_dir(root)
        .output()
        .unwrap();
    fs::remove_file(policy).ok();
    assert!(output.status.success(), "{}", diagnostic(&output));
}

#[test]
fn req_lint_rejects_duplicate_optional_mechanism_and_experiment_ids() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import copy, sys
sys.path.insert(0, sys.argv[1] + '/scripts')
from req_lint_lib.governance import check_semantics
from req_lint_lib.loader import load_registry_dir
from yaml_utils import load_yaml_file
records, _ = load_registry_dir(sys.argv[1] + '/requirements/registry.d')
policy = load_yaml_file(sys.argv[1] + '/requirements/policy/optional.yaml')
for field in ('id', 'experiment_id'):
    changed = copy.deepcopy(policy)
    changed['mechanisms'][1][field] = changed['mechanisms'][0][field]
    import yaml, tempfile, os
    handle = tempfile.NamedTemporaryFile(mode='w', suffix='.yaml', delete=False)
    yaml.safe_dump(changed, handle, sort_keys=False)
    handle.close()
    messages = '\n'.join(f.message for f in check_semantics(records, sys.argv[1] + '/RFC.md', optional_policy_path=handle.name))
    os.unlink(handle.name)
    assert 'duplicate ' + field in messages, messages
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", diagnostic(&output));
}

#[test]
fn req_lint_rejects_unrelated_promise_attestation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = temp_path("attestations");
    fs::create_dir(&directory).unwrap();
    let raw = br#"{"evidence_id":"EV-1.04-C-999","evidence_job":"req-lint","commit_sha":"1111111111111111111111111111111111111111","workflow_ref":"owner/repo/.github/workflows/ci.yaml@refs/tags/v1","run_id":"42","artifact_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","environment_digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;
    fs::write(directory.join("unrelated.json"), raw).unwrap();
    let trusted = format!("{:x}", Sha256::digest(raw));
    let script = r#"
import sys
sys.path.insert(0, sys.argv[1] + '/scripts')
from req_lint_lib.governance import check_release
record = {'id': 'KIT-GOV-001', 'record_class': 'promise', 'status': 'implemented',
          'applicability': 'mandatory', 'modality': 'declarative', 'criticality': 'blocking',
          'area': 'KIT-GOV', 'evidence_id': 'EV-1.04-C-001', 'evidence_job': 'req-lint',
          'artifact_digest': 'a' * 64, 'environment_digest': 'b' * 64}
messages = '\n'.join(f.message for f in check_release([record], attestation_dir=sys.argv[2], root=sys.argv[1]))
print(messages)
raise SystemExit(0 if 'no trusted exact evidence attestation' in messages else 1)
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .arg(&directory)
        .env("KIT_TRUSTED_ATTESTATION_SHA256", trusted)
        .env("GITHUB_SHA", "1111111111111111111111111111111111111111")
        .env(
            "KIT_ATTESTATION_WORKFLOW_REF",
            "owner/repo/.github/workflows/ci.yaml@refs/tags/v1",
        )
        .env("KIT_ATTESTATION_RUN_ID", "42")
        .current_dir(root)
        .output()
        .unwrap();
    fs::remove_dir_all(directory).ok();
    assert!(output.status.success(), "{}", diagnostic(&output));
}

#[test]
fn requirement_report_counts_are_dynamic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import sys
sys.path.insert(0, sys.argv[1] + '/scripts')
from generate_registry import report
records = [
 {'record_class':'promise','status':'implemented','applicability':'mandatory','latest_result':'pass'},
 {'record_class':'promise','status':'proposed','applicability':'mandatory','latest_result':'pending'},
 {'record_class':'requirement','status':'implemented','applicability':'mandatory','latest_result':'pass'},
]
text = report(records, {'atoms': []}, {'records': []}, ['## 1. One', '## 2. Two'])
assert '- RFC sections: 2/2' in text
assert '- Architectural promises resolved: 1/2' in text
assert '- promise: 2' in text and '- requirement: 1' in text
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", diagnostic(&output));
}
