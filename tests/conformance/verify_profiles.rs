use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use kit::verify::profiles::{
    CommitDecision, RequiredOutcome, VerificationProfile, VerificationRegistry, declared_decision,
    postcommit_decision,
};

#[test]
fn public_profile_matrix_is_ten_for_ten() {
    let mut cells = 0;
    for profile in VerificationProfile::ALL {
        for outcome in [RequiredOutcome::Pass, RequiredOutcome::Fail] {
            let expected =
                if profile == VerificationProfile::None || outcome == RequiredOutcome::Pass {
                    CommitDecision::Commit
                } else {
                    CommitDecision::Abort
                };
            assert_eq!(declared_decision(profile, outcome), expected);
            cells += 1;
        }
    }
    assert_eq!(cells, 10);
    assert!(VerificationRegistry::empty().is_empty());
    assert_eq!(
        postcommit_decision(RequiredOutcome::Fail),
        CommitDecision::AlreadyCommittedWithFailure
    );
}

#[test]
fn downstream_debug_api_cannot_forge_checks_bindings_evidence_or_passes() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let deps = manifest.join("target/debug/deps");
    let library = newest_rlib(&deps).expect("kit rlib");
    let root = std::env::temp_dir().join(format!(
        "kit-verify-api-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::create_dir_all(&root).unwrap();
    let source = root.join("forge.rs");
    fs::write(
        &source,
        r#"
use kit::executor::check::{CheckCommand, CheckRunner};
use kit::verify::profiles::{StageBinding, StagingSyntaxEvidence, VerificationAuthority, VerificationBudget};
use kit::verify::profiles::{VerificationResult, VerificationResultPayload};
use kit::workspace::edit::stage::{AbortedStagedEdit, StagedEdit};
fn main() {
    let _ = CheckCommand::new;
    let _ = CheckRunner::conformance(std::iter::empty());
    let _ = StagedEdit::materialize;
    let _ = AbortedStagedEdit::materialize;
    let _ = VerificationResult::persist_payload;
    let _ = std::mem::size_of::<VerificationResultPayload>();
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
    let _ = fs::remove_dir_all(&root);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no `StageBinding`") || stderr.contains("unresolved imports"));
    assert!(
        stderr.contains("private associated function")
            || stderr.contains("no function or associated item named `conformance`")
    );
    assert!(
        stderr
            .matches("no function or associated item named `materialize`")
            .count()
            >= 2
    );
    assert!(stderr.contains("VerificationResultPayload") && stderr.contains("private"));
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
