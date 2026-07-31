use std::collections::{BTreeMap, BTreeSet};

use kit::{
    executor::{check::CheckCommand, profile::ResourceLimits},
    verify::affected::{
        AffectedCheck, AffectedError, AffectedInput, AffectedLimits, CheckSelectionPolicy,
        ModelProposalDisposition, ModelRejection, SelectionReason, select_affected,
    },
    verify::profiles::{
        CheckClass, CheckRequirement, DeclaredCheck, ProfileSelection, VerificationRegistry,
    },
    workspace::edit::ir::RootRelativePath,
};
use serde_json::json;

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn paths(values: &[&str]) -> BTreeSet<RootRelativePath> {
    values
        .iter()
        .map(|value| RootRelativePath::parse(*value, 256).unwrap())
        .collect()
}

fn check(id: &str, policy: CheckSelectionPolicy, packages: &[&str]) -> AffectedCheck {
    AffectedCheck::new(id, policy, set(packages)).unwrap()
}

fn registry(specs: &[(&str, &[&str])]) -> VerificationRegistry {
    VerificationRegistry::new(
        specs
            .iter()
            .map(|(id, prefixes)| {
                let command = CheckCommand::new(
                    *id,
                    "cargo",
                    vec!["test".to_owned()],
                    format!("example.invalid/check@sha256:{}", "a".repeat(64)),
                    format!("sha256:{}", "b".repeat(64)),
                    format!("blake3:{}", "c".repeat(64)),
                    ResourceLimits::new(1_000, 1024, 1, 1024, 1024, 1024, 1024, 1_000),
                )
                .unwrap();
                DeclaredCheck::new(
                    CheckClass::Targeted,
                    command,
                    CheckRequirement::Required,
                    prefixes.iter().map(|value| (*value).to_owned()).collect(),
                    false,
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap()
}

fn empty_input<'a>(
    empty_paths: &'a BTreeSet<RootRelativePath>,
    empty: &'a BTreeSet<String>,
    explicit: &'a BTreeSet<String>,
    model_proposal: Option<&'a [u8]>,
) -> AffectedInput<'a> {
    AffectedInput {
        changed_paths: empty_paths,
        changed_packages: empty,
        symbol_tests: empty,
        build_dependents: empty,
        historical_checks: empty,
        prior_failure_checks: empty,
        coverage_checks: empty,
        explicit_checks: explicit,
        model_proposal,
    }
}

#[test]
fn deterministic_sources_select_and_protect_affected_checks() {
    let checks = [
        check("critical", CheckSelectionPolicy::Critical, &[]),
        check("path", CheckSelectionPolicy::RequiredWhenAffected, &[]),
        check(
            "package",
            CheckSelectionPolicy::RequiredWhenAffected,
            &["kit-api"],
        ),
        check("symbol", CheckSelectionPolicy::Optional, &[]),
        check("build", CheckSelectionPolicy::Optional, &[]),
        check("history", CheckSelectionPolicy::Optional, &[]),
        check("failure", CheckSelectionPolicy::Optional, &[]),
        check("coverage", CheckSelectionPolicy::Optional, &[]),
        check("explicit", CheckSelectionPolicy::Optional, &[]),
    ];
    let registry = registry(&[
        ("critical", &["unmatched"]),
        ("path", &["src/api"]),
        ("package", &["unmatched"]),
        ("symbol", &["unmatched"]),
        ("build", &["unmatched"]),
        ("history", &["unmatched"]),
        ("failure", &["unmatched"]),
        ("coverage", &["unmatched"]),
        ("explicit", &["unmatched"]),
    ]);
    let changed_paths = paths(&["src/api/service.rs"]);
    let packages = set(&["kit-api"]);
    let symbol = set(&["symbol"]);
    let build = set(&["build"]);
    let history = set(&["history"]);
    let failure = set(&["failure"]);
    let coverage = set(&["coverage"]);
    let explicit = set(&["explicit"]);
    let selection = select_affected(
        &registry,
        &checks,
        AffectedInput {
            changed_paths: &changed_paths,
            changed_packages: &packages,
            symbol_tests: &symbol,
            build_dependents: &build,
            historical_checks: &history,
            prior_failure_checks: &failure,
            coverage_checks: &coverage,
            explicit_checks: &explicit,
            model_proposal: None,
        },
        AffectedLimits::default(),
    )
    .unwrap();

    assert_eq!(
        selection.exact_targets(),
        &set(&[
            "build", "coverage", "critical", "explicit", "failure", "history", "package", "path",
            "symbol",
        ])
    );
    assert_eq!(
        selection.protected_floor(),
        &set(&["critical", "explicit", "package", "path"])
    );
    let expected_reasons = BTreeMap::from([
        ("build", SelectionReason::BuildDependency),
        ("coverage", SelectionReason::Coverage),
        ("critical", SelectionReason::CriticalPolicy),
        ("explicit", SelectionReason::Explicit),
        ("failure", SelectionReason::PriorFailure),
        ("history", SelectionReason::History),
        ("package", SelectionReason::Package),
        ("path", SelectionReason::ChangedPath),
        ("symbol", SelectionReason::SymbolTest),
    ]);
    for (id, reason) in expected_reasons {
        assert!(selection.reasons()[id].contains(&reason));
    }
    assert!(matches!(
        selection.clone().into_profile_selection(),
        Some(ProfileSelection::Targeted { exact_targets }) if exact_targets == *selection.exact_targets()
    ));
}

#[test]
fn model_cannot_remove_the_protected_floor_over_4096_proposals() {
    let checks = [
        check("critical", CheckSelectionPolicy::Critical, &[]),
        check("affected", CheckSelectionPolicy::RequiredWhenAffected, &[]),
        check("explicit", CheckSelectionPolicy::Optional, &[]),
        check("optional", CheckSelectionPolicy::Optional, &[]),
    ];
    let registry = registry(&[
        ("critical", &["unmatched"]),
        ("affected", &["src"]),
        ("explicit", &["unmatched"]),
        ("optional", &["unmatched"]),
    ]);
    let changed_paths = paths(&["src/lib.rs"]);
    let empty = BTreeSet::new();
    let explicit = set(&["explicit"]);
    let mut accepted = 0;
    let mut rejected = 0;
    let mut distinct = BTreeSet::new();

    for index in 0..4096 {
        let proposal = match index % 8 {
            0 => json!({"version": 1, "select": []}).to_string(),
            1 => json!({"version": 1, "select": ["optional"]}).to_string(),
            2 => json!({"version": 1, "select": ["optional", "optional"]}).to_string(),
            3 => json!({"version": 1, "select": [format!("unknown-{index}")]}).to_string(),
            4 => json!({"version": 1, "select": [], "exclude": ["critical", index]}).to_string(),
            5 => format!("{{\"version\":1,\"select\":[{index}"),
            6 => json!({"version": index + 2, "select": ["optional"]}).to_string(),
            _ => json!({"version": 1, "select": ["critical"]}).to_string(),
        };
        distinct.insert(proposal.clone());
        let selection = select_affected(
            &registry,
            &checks,
            AffectedInput {
                changed_paths: &changed_paths,
                changed_packages: &empty,
                symbol_tests: &empty,
                build_dependents: &empty,
                historical_checks: &empty,
                prior_failure_checks: &empty,
                coverage_checks: &empty,
                explicit_checks: &explicit,
                model_proposal: Some(proposal.as_bytes()),
            },
            AffectedLimits::default(),
        )
        .unwrap();

        assert!(
            selection
                .protected_floor()
                .is_subset(selection.exact_targets())
        );
        assert!(selection.exact_targets().contains("critical"));
        assert!(selection.exact_targets().contains("affected"));
        assert!(selection.exact_targets().contains("explicit"));
        match selection.model_disposition() {
            ModelProposalDisposition::Accepted => accepted += 1,
            ModelProposalDisposition::Rejected(_) => rejected += 1,
            ModelProposalDisposition::Absent => panic!("proposal was not inspected"),
        }
    }

    assert_eq!(accepted, 2048);
    assert_eq!(rejected, 2048);
    assert!(distinct.len() >= 1000);
}

#[test]
fn unknown_trusted_evidence_fails_closed() {
    let checks = [check(
        "known",
        CheckSelectionPolicy::RequiredWhenAffected,
        &[],
    )];
    let registry = registry(&[("known", &["unmatched"])]);
    let empty_paths = BTreeSet::new();
    let empty = BTreeSet::new();
    let explicit = set(&["unknown"]);

    assert!(
        select_affected(
            &registry,
            &checks,
            empty_input(&empty_paths, &empty, &explicit, None),
            AffectedLimits::default(),
        )
        .is_err()
    );
}

#[test]
fn proposal_and_floor_bounds_fail_closed_without_truncation() {
    let checks = [
        check("critical", CheckSelectionPolicy::Critical, &[]),
        check("optional", CheckSelectionPolicy::Optional, &[]),
    ];
    let registry = registry(&[("critical", &["unmatched"]), ("optional", &["unmatched"])]);
    let empty_paths = BTreeSet::new();
    let empty = BTreeSet::new();
    let explicit = BTreeSet::new();
    let oversized = vec![b' '; 17 * 1024];
    let selection = select_affected(
        &registry,
        &checks,
        empty_input(&empty_paths, &empty, &explicit, Some(&oversized)),
        AffectedLimits::default(),
    )
    .unwrap();
    assert_eq!(
        selection.model_disposition(),
        ModelProposalDisposition::Rejected(ModelRejection::TooLarge)
    );
    assert_eq!(selection.exact_targets(), &set(&["critical"]));

    assert_eq!(
        select_affected(
            &registry,
            &checks,
            empty_input(&empty_paths, &empty, &explicit, None),
            AffectedLimits {
                max_checks: 0,
                max_model_bytes: 1,
                ..AffectedLimits::default()
            },
        ),
        Err(AffectedError::InvalidLimits)
    );
    let explicit_optional = set(&["optional"]);
    assert_eq!(
        select_affected(
            &registry,
            &checks,
            empty_input(&empty_paths, &empty, &explicit_optional, None),
            AffectedLimits {
                max_checks: 1,
                max_model_bytes: 1,
                ..AffectedLimits::default()
            },
        ),
        Err(AffectedError::ProtectedFloorExceedsLimit)
    );

    let proposal = json!({"version": 1, "select": ["optional"]}).to_string();
    let selection = select_affected(
        &registry,
        &checks,
        empty_input(&empty_paths, &empty, &explicit, Some(proposal.as_bytes())),
        AffectedLimits {
            max_checks: 1,
            ..AffectedLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        selection.model_disposition(),
        ModelProposalDisposition::Rejected(ModelRejection::TooManyChecks)
    );
    assert_eq!(selection.exact_targets(), &set(&["critical"]));

    let too_many_packages = (0..4097)
        .map(|index| format!("package-{index}"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        select_affected(
            &registry,
            &checks,
            AffectedInput {
                changed_paths: &empty_paths,
                changed_packages: &too_many_packages,
                symbol_tests: &empty,
                build_dependents: &empty,
                historical_checks: &empty,
                prior_failure_checks: &empty,
                coverage_checks: &empty,
                explicit_checks: &explicit,
                model_proposal: None,
            },
            AffectedLimits::default(),
        ),
        Err(AffectedError::SelectionExceedsLimit)
    );
}
