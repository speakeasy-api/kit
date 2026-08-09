#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    path::{Path, PathBuf},
};

use kit::{
    domain::ids::{PrincipalId, ProjectId},
    store::artifacts::{ArtifactRetention, ArtifactStore},
    workspace::{
        edit::{
            format::{RUST_GRAMMAR_VERSION, SyntaxRequirement},
            ir::{EditIr, EditLimits},
            recovery::MaterializeOptions,
            stage::{StageLimits, stage},
            validate::{ValidationError, validate_authorized},
        },
        index::meta::{IndexOptions, MetadataIndex},
        revision::ManagedWorkspace,
        search::structural::{StructuralError, StructuralOptions, StructuralQuery, search},
        syntax::{SyntaxCacheLimits, SyntaxIndex},
    },
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
    workspace: ManagedWorkspace,
    artifacts: ArtifactStore,
    principal: PrincipalId,
    project: ProjectId,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-structural-{}",
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).unwrap();
        for (path, source) in files {
            let path = workspace_path.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let workspace = ManagedWorkspace::open(&workspace_path).unwrap();
        Self {
            root,
            workspace_path,
            workspace,
            artifacts,
            principal: PrincipalId::generate().unwrap(),
            project: ProjectId::generate().unwrap(),
        }
    }

    fn index(&self, syntax: &mut SyntaxIndex) -> MetadataIndex {
        let revision = self.workspace.current_revision().unwrap().id();
        MetadataIndex::build_with_syntax(
            &self.workspace,
            revision,
            &IndexOptions::default(),
            syntax,
        )
        .unwrap()
    }

    fn preview(
        &self,
        pattern: &str,
        rewrite: Option<&str>,
    ) -> kit::workspace::search::structural::StructuralResponse {
        let mut syntax = SyntaxIndex::new();
        let index = self.index(&mut syntax);
        search(
            &self.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: pattern.to_owned(),
                rewrite: rewrite.map(str::to_owned),
            },
            &StructuralOptions::default(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn structural_search_is_parser_derived_bounded_and_revision_tagged() {
    let fixture = Fixture::new(&[
        (
            "src/lib.rs",
            "fn f() { let a = Some(1); let b = Some(2); let s = \"Some(3)\"; /* Some(4) */ }\n",
        ),
        ("notes.txt", "Some(5)\n"),
    ]);
    let mut syntax = SyntaxIndex::new();
    let index = fixture.index(&mut syntax);
    let syntax_metrics = syntax.metrics();
    let response = search(
        &fixture.workspace,
        &index,
        &mut syntax,
        &StructuralQuery {
            pattern: "Some($A)".to_owned(),
            rewrite: None,
        },
        &StructuralOptions {
            max_matches: 1,
            ..StructuralOptions::default()
        },
    )
    .unwrap();

    assert_eq!(response.revision, index.revision());
    assert_eq!(response.matches.len(), 1);
    assert_eq!(response.matches[0].path, Path::new("src/lib.rs"));
    assert_eq!(response.matches[0].text, "Some(1)");
    assert_eq!(response.matches[0].captures[0].text, "1");
    assert_eq!(response.matches[0].provenance.revision, index.revision());
    assert_eq!(syntax.metrics(), syntax_metrics);
    assert!(response.truncated);
    assert!(!response.omitted_complete);
}

#[test]
fn rewrite_preview_is_canonical_ir_and_leaves_workspace_untouched() {
    let fixture = Fixture::new(&[("src/lib.rs", "fn f() { let value = Some(1); }\n")]);
    let before = fs::read(fixture.workspace_path.join("src/lib.rs")).unwrap();
    let response = fixture.preview("Some($A)", Some("Ok($A)"));
    let serialized = serde_json::to_value(&response).unwrap();
    let rewrite = response.rewrite.unwrap();

    assert_eq!(
        fs::read(fixture.workspace_path.join("src/lib.rs")).unwrap(),
        before
    );
    assert_eq!(rewrite.ir_digest, rewrite.ir.digest());
    assert_eq!(
        EditIr::from_canonical_bytes(&rewrite.ir.canonical_bytes(), EditLimits::default()).unwrap(),
        rewrite.ir
    );
    assert_eq!(rewrite.ir.operations().len(), 1);
    assert!(serialized["rewrite"].get("apply").is_none());
    assert!(serialized["rewrite"].get("ir").is_none());
    assert!(
        rewrite
            .change_diff
            .contains("-fn f() { let value = Some(1); }")
    );
    assert!(
        rewrite
            .change_diff
            .contains("+fn f() { let value = Ok(1); }")
    );
}

#[test]
fn identity_rewrite_is_an_explicit_no_change_plan() {
    let fixture = Fixture::new(&[("src/lib.rs", "fn f() { Some(1); }\n")]);
    let response = fixture.preview("Some($A)", Some("Some($A)"));
    let serialized = serde_json::to_value(&response).unwrap();
    let rewrite = response.rewrite.unwrap();

    assert!(!rewrite.changed);
    assert!(rewrite.ir.operations().is_empty());
    assert!(rewrite.change_diff.is_empty());
    assert!(serialized["rewrite"].get("apply").is_none());
}

#[test]
fn hostile_nested_and_repeated_matches_keep_retained_and_serialized_output_bounded() {
    let nested = format!(
        "fn nested() {{ Some({}1{}); }}\n",
        "(".repeat(300),
        ")".repeat(300)
    );
    let repeated = format!("fn repeated() {{ {} }}\n", "Some(123); ".repeat(500));
    for source in [nested, repeated] {
        let fixture = Fixture::new(&[("src/lib.rs", &source)]);
        let mut syntax = SyntaxIndex::new();
        let index = fixture.index(&mut syntax);
        let response = search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: None,
            },
            &StructuralOptions {
                max_output_bytes: 4 * 1024,
                max_matches: 1_000,
                ..StructuralOptions::default()
            },
        )
        .unwrap();
        let bytes = serde_json::to_vec(&response).unwrap();
        assert_eq!(response.result_bytes, bytes.len());
        assert!(bytes.len() <= 4 * 1024);
        assert!(response.truncated);
        assert!(response.omitted > 0);
        assert!(
            response
                .matches
                .iter()
                .map(|matched| {
                    matched.text.len()
                        + matched
                            .captures
                            .iter()
                            .map(|capture| capture.name.len() + capture.text.len())
                            .sum::<usize>()
                })
                .sum::<usize>()
                < 4 * 1024
        );
    }
}

#[test]
fn oversized_rewrite_response_fails_closed_without_partial_ir() {
    let source = format!("fn f() {{ Some({}1); }}\n", "1 + ".repeat(2_000));
    let fixture = Fixture::new(&[("src/lib.rs", &source)]);
    let mut syntax = SyntaxIndex::new();
    let index = fixture.index(&mut syntax);
    assert!(matches!(
        search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some("Ok($A)".to_owned()),
            },
            &StructuralOptions {
                max_output_bytes: 4 * 1024,
                ..StructuralOptions::default()
            },
        ),
        Err(StructuralError::IncompleteRewrite(_))
    ));
}

#[test]
fn preview_change_diff_equals_materialized_change_diff() {
    let fixture = Fixture::new(&[("src/lib.rs", "fn f() { let value = Some(1); }\n")]);
    let rewrite = fixture.preview("Some($A)", Some("Ok($A)")).rewrite.unwrap();
    let plan = validate_authorized(
        &fixture.workspace,
        &rewrite.ir,
        EditLimits::default(),
        kit::test_support::trusted_edit_authority(fixture.principal, fixture.project),
    )
    .unwrap();
    let requirement = SyntaxRequirement::new(
        kit::workspace::edit::ir::RootRelativePath::parse(
            "src/lib.rs",
            EditLimits::default().max_path_bytes,
        )
        .unwrap(),
        "rust",
        RUST_GRAMMAR_VERSION,
        true,
    )
    .unwrap();
    let mut syntax = kit::test_support::syntax_executor(
        "rust",
        RUST_GRAMMAR_VERSION,
        kit::test_support::SyntaxTestAction::Pass,
    );
    let staged = stage(
        plan,
        StageLimits::default(),
        &[requirement],
        &mut [&mut syntax],
    )
    .unwrap();
    let materialized = staged
        .materialize(
            &fixture.artifacts,
            MaterializeOptions::new(ArtifactRetention::Forever),
        )
        .unwrap();

    assert!(materialized.change_diff_complete());
    assert_eq!(rewrite.change_diff.as_bytes(), materialized.change_diff());
    assert_eq!(
        fs::read_to_string(fixture.workspace_path.join("src/lib.rs")).unwrap(),
        "fn f() { let value = Ok(1); }\n"
    );
}

#[test]
fn post_preview_leaf_and_ancestor_symlink_races_are_denied() {
    for ancestor in [false, true] {
        let fixture = Fixture::new(&[("src/lib.rs", "fn f() { let value = Some(1); }\n")]);
        let rewrite = fixture.preview("Some($A)", Some("Ok($A)")).rewrite.unwrap();
        let outside = fixture.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("lib.rs");
        fs::write(&sentinel, "outside sentinel\n").unwrap();
        if ancestor {
            fs::rename(
                fixture.workspace_path.join("src"),
                fixture.workspace_path.join("src-real"),
            )
            .unwrap();
            std::os::unix::fs::symlink(&outside, fixture.workspace_path.join("src")).unwrap();
        } else {
            fs::remove_file(fixture.workspace_path.join("src/lib.rs")).unwrap();
            std::os::unix::fs::symlink(&sentinel, fixture.workspace_path.join("src/lib.rs"))
                .unwrap();
        }

        let denied = validate_authorized(
            &fixture.workspace,
            &rewrite.ir,
            EditLimits::default(),
            kit::test_support::trusted_edit_authority(fixture.principal, fixture.project),
        );
        assert!(matches!(
            denied,
            Err(ValidationError::StaleRevision
                | ValidationError::UnsafePath(_)
                | ValidationError::PathStateMismatch)
        ));
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "outside sentinel\n");
    }
}

#[test]
fn rewrite_rejects_undefined_variables_and_overlapping_matches() {
    let fixture = Fixture::new(&[("src/lib.rs", "fn f() { Some(Some(1)); }\n")]);
    let mut syntax = SyntaxIndex::new();
    let index = fixture.index(&mut syntax);
    let mut run = |rewrite: &str| {
        search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some(rewrite.to_owned()),
            },
            &StructuralOptions::default(),
        )
    };
    assert!(matches!(
        run("Ok($B)"),
        Err(StructuralError::InvalidQuery(_))
    ));
    assert!(matches!(
        run("Ok($A)"),
        Err(StructuralError::AmbiguousRewrite(_))
    ));
}

#[test]
fn rewrite_rejects_malformed_incomplete_and_stale_inputs() {
    let malformed = Fixture::new(&[("src/lib.rs", "fn f(\n")]);
    let mut malformed_syntax = SyntaxIndex::new();
    let malformed_index = malformed.index(&mut malformed_syntax);
    assert!(matches!(
        search(
            &malformed.workspace,
            &malformed_index,
            &mut malformed_syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some("Ok($A)".to_owned()),
            },
            &StructuralOptions::default(),
        ),
        Err(StructuralError::MalformedSource(_))
    ));

    let fixture = Fixture::new(&[("src/lib.rs", "fn f() { Some(1); }\n")]);
    let revision = fixture.workspace.current_revision().unwrap().id();
    let truncated = MetadataIndex::build(
        &fixture.workspace,
        revision,
        &IndexOptions {
            max_entries: 1,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    let mut truncated_syntax = SyntaxIndex::new();
    assert!(matches!(
        search(
            &fixture.workspace,
            &truncated,
            &mut truncated_syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some("Ok($A)".to_owned()),
            },
            &StructuralOptions::default(),
        ),
        Err(StructuralError::IncompleteRewrite(_))
    ));

    let mut syntax = SyntaxIndex::new();
    let index = fixture.index(&mut syntax);
    let mut invalid = |pattern: &str, rewrite: &str| {
        search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: pattern.to_owned(),
                rewrite: Some(rewrite.to_owned()),
            },
            &StructuralOptions::default(),
        )
    };
    assert!(matches!(
        invalid("Some(", "Ok($A)"),
        Err(StructuralError::InvalidQuery(_))
    ));
    assert!(matches!(
        invalid("Some($A)", "Ok("),
        Err(StructuralError::InvalidQuery(_))
    ));
    fs::write(
        fixture.workspace_path.join("src/lib.rs"),
        "fn changed() {}\n",
    )
    .unwrap();
    assert!(matches!(
        search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some("Ok($A)".to_owned()),
            },
            &StructuralOptions::default(),
        ),
        Err(StructuralError::Revision(_))
    ));
}

#[test]
fn structural_selection_reports_unavailable_rust_sources_and_rewrite_fails_closed() {
    for (bytes, options, skipped) in [
        (
            b"fn target() { Some(1); }\n".repeat(8),
            IndexOptions {
                max_file_bytes: 32,
                ..IndexOptions::default()
            },
            "too_large",
        ),
        (
            vec![b'f', b'n', b' ', 0xff, b'\n'],
            IndexOptions::default(),
            "invalid_utf8",
        ),
    ] {
        let fixture = Fixture::new(&[("src/lib.rs", "placeholder\n")]);
        fs::write(fixture.workspace_path.join("src/lib.rs"), bytes).unwrap();
        let revision = fixture.workspace.current_revision().unwrap().id();
        let mut syntax = SyntaxIndex::new();
        let index =
            MetadataIndex::build_with_syntax(&fixture.workspace, revision, &options, &mut syntax)
                .unwrap();
        let query = StructuralQuery {
            pattern: "Some($A)".to_owned(),
            rewrite: None,
        };
        let response = search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &query,
            &StructuralOptions::default(),
        )
        .unwrap();
        assert!(response.truncated);
        assert!(!response.omitted_complete);
        assert_eq!(response.omitted, 1);
        assert_eq!(
            serde_json::to_value(response).unwrap()["skipped"][skipped],
            1
        );
        assert!(matches!(
            search(
                &fixture.workspace,
                &index,
                &mut syntax,
                &StructuralQuery {
                    pattern: "Some($A)".to_owned(),
                    rewrite: Some("Ok($A)".to_owned()),
                },
                &StructuralOptions::default(),
            ),
            Err(StructuralError::IncompleteRewrite(_))
        ));
    }

    let fixture = Fixture::new(&[("src/lib.rs", "fn target() { Some(1); }\n")]);
    let mut syntax = SyntaxIndex::new();
    let index = fixture.index(&mut syntax);
    let options = StructuralOptions {
        max_source_bytes: 8,
        ..StructuralOptions::default()
    };
    let response = search(
        &fixture.workspace,
        &index,
        &mut syntax,
        &StructuralQuery {
            pattern: "Some($A)".to_owned(),
            rewrite: None,
        },
        &options,
    )
    .unwrap();
    assert_eq!(response.skipped.too_large, 1);
    assert!(response.truncated);
    assert!(matches!(
        search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some("Ok($A)".to_owned()),
            },
            &options,
        ),
        Err(StructuralError::IncompleteRewrite(_))
    ));
}

#[test]
fn symbol_truncation_and_tree_eviction_do_not_hide_complete_structural_source() {
    let mut source = (0..300)
        .map(|index| format!("fn symbol_{index}() {{}}\n"))
        .collect::<String>();
    source.push_str("fn target() { Some(1); }\n");
    let fixture = Fixture::new(&[("src/a.rs", &source), ("src/b.rs", "fn b() { Some(2); }\n")]);
    let revision = fixture.workspace.current_revision().unwrap().id();
    let mut syntax = SyntaxIndex::with_cache_limits(SyntaxCacheLimits {
        max_resident_files: 1,
        ..SyntaxCacheLimits::default()
    })
    .unwrap();
    let index = MetadataIndex::build_with_syntax(
        &fixture.workspace,
        revision,
        &IndexOptions::default(),
        &mut syntax,
    )
    .unwrap();
    assert!(index.truncated());
    assert!(!index.source_truncated());
    assert_eq!(syntax.cache_usage().resident_files, 1);
    let parses = syntax.metrics().full_parses;
    let response = search(
        &fixture.workspace,
        &index,
        &mut syntax,
        &StructuralQuery {
            pattern: "Some($A)".to_owned(),
            rewrite: Some("Ok($A)".to_owned()),
        },
        &StructuralOptions::default(),
    )
    .unwrap();
    assert!(response.rewrite.unwrap().changed);
    assert_eq!(response.matches.len(), 2);
    assert!(syntax.metrics().full_parses > parses);
}

#[test]
fn repeated_metavariable_expansion_is_rejected_before_replacement_allocation() {
    let expression = format!("{}1", "1 + ".repeat(500));
    let fixture = Fixture::new(&[("src/lib.rs", &format!("fn f() {{ Some({expression}); }}\n"))]);
    let mut syntax = SyntaxIndex::new();
    let index = fixture.index(&mut syntax);
    let replacement = format!("({})", vec!["$A"; 16].join(", "));
    assert!(matches!(
        search(
            &fixture.workspace,
            &index,
            &mut syntax,
            &StructuralQuery {
                pattern: "Some($A)".to_owned(),
                rewrite: Some(replacement),
            },
            &StructuralOptions {
                max_rewrite_bytes: 8 * 1024,
                ..StructuralOptions::default()
            },
        ),
        Err(StructuralError::IncompleteRewrite(
            "replacement expansion exceeds a rewrite, output, or IR bound"
        ))
    ));
}
