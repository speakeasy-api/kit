use kit::{
    domain::events::ContentDigest,
    workspace::edit::{
        ir::{
            ByteRange, EDIT_IR_VERSION, EditIr, EditLimits, EditOperation, ExecutableMode,
            FilesystemIdentityPolicy, IrError, Newline, RevisionToken, RootRelativePath,
            TextContent,
        },
        normalize::{
            ModelEditFormat, NormalizationContext, NormalizeError, normalize,
            normalize_structured_json, normalize_whole_file,
        },
    },
};
use serde_json::json;
use std::collections::BTreeSet;

fn revision() -> RevisionToken {
    RevisionToken::parse(format!("r:{}", "1".repeat(64))).unwrap()
}

fn digest(seed: u64) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{seed:064x}")).unwrap()
}

fn path(value: &str) -> RootRelativePath {
    RootRelativePath::parse(value, EditLimits::default().max_path_bytes).unwrap()
}

fn text(value: &str) -> TextContent {
    TextContent::from_bytes(value.as_bytes()).unwrap()
}

fn add(value: &str) -> EditOperation {
    EditOperation::AddFile {
        path: path(value),
        content: text("x"),
        executable: false,
    }
}

fn context() -> NormalizationContext {
    NormalizationContext::new(revision(), EditLimits::default())
}

#[test]
fn canonical_ir_has_exactly_the_four_operations_and_explicit_semantics() {
    let operations = vec![
        EditOperation::AddFile {
            path: path("new.rs"),
            content: text("fn new() {}\n"),
            executable: false,
        },
        EditOperation::DeleteFile {
            path: path("old.rs"),
            base_digest: digest(1),
        },
        EditOperation::MoveFile {
            from: path("from.rs"),
            to: path("to.rs"),
            base_digest: digest(2),
        },
        EditOperation::ReplaceRange {
            path: path("edit.rs"),
            base_digest: digest(3),
            range: ByteRange::new(0, 4).unwrap(),
            expected: text("old\n"),
            replacement: text("new\r\n"),
            executable: ExecutableMode::Preserve,
        },
    ];
    let structured = serde_json::to_vec(&json!({
        "version": 1,
        "expected_revision": revision().to_string(),
        "operations": operations,
    }))
    .unwrap();
    let ir = normalize_structured_json(&structured, &context()).unwrap();
    assert_eq!(ir.version(), EDIT_IR_VERSION);
    assert_eq!(ir.operations().len(), 4);
    assert_eq!(ir.operations()[0].order(), 0);
    assert!(ir.operations()[0].id().starts_with("op:00000000:"));
    assert!(matches!(
        ir.operations()[0].operation(),
        EditOperation::AddFile { executable: false, content, .. }
            if content.newline() == Newline::Lf && content.has_final_newline()
    ));
    assert!(matches!(
        ir.operations()[1].operation(),
        EditOperation::DeleteFile { .. }
    ));
    assert!(matches!(
        ir.operations()[2].operation(),
        EditOperation::MoveFile { .. }
    ));
    assert!(matches!(
        ir.operations()[3].operation(),
        EditOperation::ReplaceRange { replacement, .. }
            if replacement.newline() == Newline::Crlf
    ));

    let vocabulary: BTreeSet<_> = ir
        .operations()
        .iter()
        .map(|operation| match operation.operation() {
            EditOperation::AddFile { .. } => "add_file",
            EditOperation::DeleteFile { .. } => "delete_file",
            EditOperation::MoveFile { .. } => "move_file",
            EditOperation::ReplaceRange { .. } => "replace_range",
        })
        .collect();
    assert_eq!(
        vocabulary,
        BTreeSet::from(["add_file", "delete_file", "move_file", "replace_range"])
    );
    let fixed = EditIr::new(
        revision(),
        vec![EditOperation::AddFile {
            path: path("vector.txt"),
            content: text("fixed\n"),
            executable: false,
        }],
        EditLimits::default(),
    )
    .unwrap();
    assert_eq!(
        fixed.canonical_bytes(),
        br#"{"version":1,"identity_policy":"portable","expected_revision":"r:1111111111111111111111111111111111111111111111111111111111111111","operations":[{"id":"op:00000000:1a7e8816909f412405f12cf566f6c55f601e3d25376fbd04032d09f596d8f98c","order":0,"op":"add_file","path":"vector.txt","content":{"encoding":"utf8","newline":"lf","text":"fixed","final_newline":true},"executable":false}]}"#
    );
    assert_eq!(
        fixed.digest(),
        "blake3:07dc48520cae3c3b4a731fb735b74924a003b77f8fe248606fd55451082fc7ed"
    );
}

#[test]
fn canonical_serialization_round_trips_10_000_generated_cases() {
    let limits = EditLimits::default();
    for case in 0_u64..10_000 {
        let unicode = ["alpha", "lambda-λ", "東京", "emoji-🙂"][(case as usize) % 4];
        let ending = if case & 1 == 0 { "\n" } else { "\r\n" };
        let final_newline = match case & 10 {
            0 => ending.repeat(2),
            2 => ending.to_owned(),
            _ => String::new(),
        };
        let body = format!("{unicode}-{case}{final_newline}");
        let expected = text(&body);
        let second_expected = text("tail");
        let mode = [
            ExecutableMode::Preserve,
            ExecutableMode::Executable,
            ExecutableMode::NonExecutable,
        ][case as usize % 3];
        let operations = vec![
            EditOperation::AddFile {
                path: path(&format!("generated/add-{case}.txt")),
                content: text(&body),
                executable: case & 4 != 0,
            },
            EditOperation::DeleteFile {
                path: path(&format!("generated/delete-{case}.txt")),
                base_digest: digest(case),
            },
            EditOperation::MoveFile {
                from: path(&format!("generated/{case}.old")),
                to: path(&format!("generated/{case}.new")),
                base_digest: digest(case + 1),
            },
            EditOperation::ReplaceRange {
                path: path(&format!("generated/edit-{case}.txt")),
                base_digest: digest(case + 2),
                range: ByteRange::new(0, expected.rendered_len()).unwrap(),
                expected,
                replacement: text(&format!("replacement-{unicode}{ending}")),
                executable: mode,
            },
            EditOperation::ReplaceRange {
                path: path(&format!("generated/edit-{case}.txt")),
                base_digest: digest(case + 2),
                range: ByteRange::new(body.len(), body.len() + second_expected.rendered_len())
                    .unwrap(),
                expected: second_expected,
                replacement: text("end"),
                executable: mode,
            },
        ];
        let original = EditIr::new(revision(), operations.clone(), limits).unwrap();
        let bytes = original.canonical_bytes();
        let decoded = EditIr::from_canonical_bytes(&bytes, limits).unwrap();
        assert_eq!(decoded, original, "generated case {case}");
        assert_eq!(decoded.digest(), original.digest(), "generated case {case}");

        let mut rejected = operations;
        rejected.push(match case % 4 {
            0 => EditOperation::AddFile {
                path: path(&format!("generated/add-{case}.txt")),
                content: text("duplicate"),
                executable: false,
            },
            1 => EditOperation::MoveFile {
                from: path(&format!("generated/{case}.new")),
                to: path(&format!("generated/{case}.old")),
                base_digest: digest(case + 3),
            },
            2 => EditOperation::ReplaceRange {
                path: path(&format!("generated/edit-{case}.txt")),
                base_digest: digest(case + 2),
                range: ByteRange::new(0, 0).unwrap(),
                expected: text(""),
                replacement: text("conflict"),
                executable: mode,
            },
            _ => EditOperation::ReplaceRange {
                path: path(&format!("generated/edit-{case}.txt")),
                base_digest: digest(case + 99),
                range: ByteRange::new(body.len() + 4, body.len() + 4).unwrap(),
                expected: text(""),
                replacement: text("conflict"),
                executable: mode,
            },
        });
        assert!(EditIr::new(revision(), rejected, limits).is_err());
    }
}

#[test]
fn canonical_digest_ignores_model_json_whitespace_and_map_order() {
    let first = format!(
        r#"{{"version":1,"expected_revision":"{}","operations":[{{"op":"add_file","path":"a","content":{{"encoding":"utf8","newline":"lf","text":"x","final_newline":false}},"executable":false}}]}}"#,
        revision()
    );
    let second = format!(
        r#"{{
          "operations": [{{"executable": false, "content": {{"text": "x", "final_newline": false, "newline": "lf", "encoding": "utf8"}}, "path": "a", "op": "add_file"}}],
          "expected_revision": "{}", "version": 1
        }}"#,
        revision()
    );
    let context = context();
    let a = normalize_structured_json(first.as_bytes(), &context).unwrap();
    let b = normalize_structured_json(second.as_bytes(), &context).unwrap();
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(a.digest(), b.digest());
    let pretty = serde_json::to_vec_pretty(
        &serde_json::from_slice::<serde_json::Value>(&a.canonical_bytes()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        EditIr::from_canonical_bytes(&pretty, EditLimits::default()),
        Err(IrError::NonCanonical)
    );

    let mut tampered: serde_json::Value = serde_json::from_slice(&a.canonical_bytes()).unwrap();
    tampered["operations"][0]["id"] = json!("op:tampered");
    assert!(matches!(
        EditIr::from_canonical_bytes(
            &serde_json::to_vec(&tampered).unwrap(),
            EditLimits::default()
        ),
        Err(IrError::NonCanonical)
    ));
    let unknown = first.replace("add_file", "copy_file");
    assert!(matches!(
        normalize_structured_json(unknown.as_bytes(), &context),
        Err(NormalizeError::MalformedJson(_))
    ));
}

#[test]
fn whole_file_unified_diff_and_structured_json_normalize_equivalently() {
    let mut context = context();
    context.insert_file("src/λ.rs", b"old\n", false).unwrap();
    let whole = format!(
        r#"{{"version":1,"expected_revision":"{}","files":[{{"path":"src/λ.rs","content":"new\n","executable":false}}]}}"#,
        revision()
    );
    let diff = b"--- a/src/\xce\xbb.rs\n+++ b/src/\xce\xbb.rs\n@@ -1 +1 @@\n-old\n+new\n";
    let base_digest = context
        .file(&path("src/λ.rs"))
        .unwrap()
        .digest()
        .to_string();
    let structured = json!({
        "operations": [{
            "replacement": {"final_newline": true, "text": "new", "newline": "lf", "encoding": "utf8"},
            "expected": {"encoding": "utf8", "newline": "lf", "text": "old", "final_newline": true},
            "range": {"end": 4, "start": 0},
            "base_digest": base_digest,
            "path": "src/λ.rs",
            "executable": "preserve",
            "op": "replace_range"
        }],
        "version": 1,
        "expected_revision": revision().to_string()
    });
    let a = normalize_whole_file(whole.as_bytes(), &context).unwrap();
    let b = normalize(ModelEditFormat::UnifiedDiff, diff, &context).unwrap();
    let c = normalize_structured_json(&serde_json::to_vec(&structured).unwrap(), &context).unwrap();
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(b.canonical_bytes(), c.canonical_bytes());
}

#[test]
fn unified_diff_handles_crlf_missing_final_newline_empty_files_and_moves() {
    let mut context = context().with_default_newline(Newline::Crlf);
    context.insert_file("crlf.txt", b"old\r\n", false).unwrap();
    context.insert_file("no-final.txt", b"bye", false).unwrap();
    context.insert_file("move-from", b"moved\n", true).unwrap();
    context.insert_file("dash", b"- \n", false).unwrap();
    context.insert_file("mode", b"mode\n", false).unwrap();
    context
        .insert_file("marker-text", b"GIT binary patch\n", false)
        .unwrap();

    let crlf = normalize(
        ModelEditFormat::UnifiedDiff,
        b"--- a/crlf.txt\r\n+++ b/crlf.txt\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        crlf.operations()[0].operation(),
        EditOperation::ReplaceRange { replacement, .. }
            if replacement.render() == b"new\r\n"
    ));

    let deleted = normalize(
        ModelEditFormat::UnifiedDiff,
        b"--- a/no-final.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\\ No newline at end of file",
        &context,
    )
    .unwrap();
    assert!(matches!(
        deleted.operations()[0].operation(),
        EditOperation::DeleteFile { .. }
    ));

    let added = normalize(
        ModelEditFormat::UnifiedDiff,
        b"--- /dev/null\n+++ b/empty\n@@ -0,0 +0,0 @@\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        added.operations()[0].operation(),
        EditOperation::AddFile { content, .. } if content.render().is_empty()
    ));

    let moved = normalize(
        ModelEditFormat::UnifiedDiff,
        b"diff --git a/move-from b/move-to\nsimilarity index 100%\nrename from move-from\nrename to move-to\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        moved.operations()[0].operation(),
        EditOperation::MoveFile { .. }
    ));

    let dash = normalize(
        ModelEditFormat::UnifiedDiff,
        b"--- a/dash\n+++ b/dash\n@@ -1 +1 @@\n-- \n+ok\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        dash.operations()[0].operation(),
        EditOperation::ReplaceRange { expected, replacement, .. }
            if expected.render() == b"- \n" && replacement.render() == b"ok\n"
    ));

    let mode = normalize(
        ModelEditFormat::UnifiedDiff,
        b"diff --git a/mode b/mode\nold mode 100644\nnew mode 100755\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        mode.operations()[0].operation(),
        EditOperation::ReplaceRange {
            expected,
            replacement,
            executable: ExecutableMode::Executable,
            ..
        } if expected.render().is_empty() && replacement.render().is_empty()
    ));

    let marker_text = normalize(
        ModelEditFormat::UnifiedDiff,
        b"--- a/marker-text\n+++ b/marker-text\n@@ -1 +1 @@\n-GIT binary patch\n+Binary files a and b differ\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        marker_text.operations()[0].operation(),
        EditOperation::ReplaceRange { replacement, .. }
            if replacement.render() == b"Binary files a and b differ\n"
    ));

    for bytes in [b"\n\n".as_slice(), b"one\r\n\r\n".as_slice()] {
        let content = TextContent::from_bytes(bytes).unwrap();
        assert_eq!(content.render(), bytes);
    }
}

#[test]
fn malformed_binary_duplicate_overlap_and_move_cycles_are_typed_rejections() {
    let mut context = context();
    context.insert_file("a", b"one\ntwo\n", false).unwrap();
    assert_eq!(
        normalize(
            ModelEditFormat::UnifiedDiff,
            b"GIT binary patch\0",
            &context
        ),
        Err(NormalizeError::BinaryPatch)
    );
    assert!(matches!(
        normalize(
            ModelEditFormat::UnifiedDiff,
            b"--- a/a\n+++ b/a\n@@ -1,2 +1 @@\n-one\n+changed\n",
            &context
        ),
        Err(NormalizeError::MalformedPatch { .. })
    ));
    for malformed in [
        b"--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n-one\n\\ No newline at end of file\n+changed\n two\n"
            .as_slice(),
        b"diff --git a/a b/other\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-one\n+changed\n".as_slice(),
        b"--- a/a\n+++ b/a\n@@ -1 +1 @@\n one\n@@ -2 +3 @@\n two\n".as_slice(),
        b"GIT binary patch suffix\n".as_slice(),
    ] {
        assert!(matches!(
            normalize(ModelEditFormat::UnifiedDiff, malformed, &context),
            Err(NormalizeError::MalformedPatch { .. }) | Err(NormalizeError::BaseMismatch(_))
        ));
    }

    let duplicate_path = vec![
        EditOperation::AddFile {
            path: path("duplicate"),
            content: text("first"),
            executable: false,
        },
        EditOperation::AddFile {
            path: path("duplicate"),
            content: text("second"),
            executable: false,
        },
    ];
    assert_eq!(
        EditIr::new(revision(), duplicate_path, EditLimits::default()),
        Err(IrError::PathConflict("duplicate".to_owned()))
    );

    let overlap = vec![
        EditOperation::ReplaceRange {
            path: path("a"),
            base_digest: digest(1),
            range: ByteRange::new(0, 4).unwrap(),
            expected: text("one\n"),
            replacement: text("x\n"),
            executable: ExecutableMode::Preserve,
        },
        EditOperation::ReplaceRange {
            path: path("a"),
            base_digest: digest(1),
            range: ByteRange::new(2, 6).unwrap(),
            expected: text("e\ntw"),
            replacement: text("y"),
            executable: ExecutableMode::Preserve,
        },
    ];
    assert_eq!(
        EditIr::new(revision(), overlap, EditLimits::default()),
        Err(IrError::OverlappingRanges("a".to_owned()))
    );
    let mode_conflict = vec![
        EditOperation::ReplaceRange {
            path: path("a"),
            base_digest: digest(1),
            range: ByteRange::new(0, 4).unwrap(),
            expected: text("one\n"),
            replacement: text("x\n"),
            executable: ExecutableMode::Executable,
        },
        EditOperation::ReplaceRange {
            path: path("a"),
            base_digest: digest(1),
            range: ByteRange::new(4, 8).unwrap(),
            expected: text("two\n"),
            replacement: text("y\n"),
            executable: ExecutableMode::Preserve,
        },
    ];
    assert_eq!(
        EditIr::new(revision(), mode_conflict, EditLimits::default()),
        Err(IrError::ExecutableModeConflict("a".to_owned()))
    );
    let cycle = vec![
        EditOperation::MoveFile {
            from: path("a"),
            to: path("b"),
            base_digest: digest(1),
        },
        EditOperation::MoveFile {
            from: path("b"),
            to: path("a"),
            base_digest: digest(2),
        },
    ];
    assert!(matches!(
        EditIr::new(revision(), cycle, EditLimits::default()),
        Err(IrError::MoveCycle(_))
    ));
}

#[test]
fn filesystem_identity_policy_rejects_portable_aliases_and_preserves_original_paths() {
    for aliases in [
        ["Foo", "foo"],
        ["É.txt", "é.txt"],
        ["é.txt", "e\u{301}.txt"],
    ] {
        assert!(matches!(
            EditIr::new(
                revision(),
                aliases.into_iter().map(add).collect(),
                EditLimits::default()
            ),
            Err(IrError::PathConflict(_))
        ));
    }

    let case_sensitive = EditLimits {
        identity_policy: FilesystemIdentityPolicy::CaseSensitive,
        ..EditLimits::default()
    };
    let ir = EditIr::new(revision(), vec![add("Foo"), add("foo")], case_sensitive).unwrap();
    assert!(matches!(
        EditIr::new(
            revision(),
            vec![add("é.txt"), add("e\u{301}.txt")],
            case_sensitive
        ),
        Err(IrError::PathConflict(_))
    ));
    assert_eq!(
        ir.identity_policy(),
        FilesystemIdentityPolicy::CaseSensitive
    );
    assert!(matches!(
        ir.operations()[0].operation(),
        EditOperation::AddFile { path, .. } if path.as_str() == "Foo"
    ));
    assert!(matches!(
        ir.operations()[1].operation(),
        EditOperation::AddFile { path, .. } if path.as_str() == "foo"
    ));
    let canonical = ir.canonical_bytes();
    assert!(
        std::str::from_utf8(&canonical)
            .unwrap()
            .contains(r#""identity_policy":"case_sensitive""#)
    );
    assert_eq!(
        EditIr::from_canonical_bytes(&canonical, case_sensitive).unwrap(),
        ir
    );
    assert!(matches!(
        EditIr::from_canonical_bytes(&canonical, EditLimits::default()),
        Err(IrError::IdentityPolicyMismatch { .. })
    ));

    let mut portable_context = context();
    portable_context.insert_file("Foo", b"x", false).unwrap();
    assert!(matches!(
        portable_context.insert_file("foo", b"x", false),
        Err(NormalizeError::DuplicatePath(_))
    ));
    let mut case_sensitive_context = NormalizationContext::new(revision(), case_sensitive);
    case_sensitive_context
        .insert_file("Foo", b"x", false)
        .unwrap();
    case_sensitive_context
        .insert_file("foo", b"x", false)
        .unwrap();
}

#[test]
fn move_graph_uses_target_filesystem_identity() {
    let alias_cycle = vec![
        EditOperation::MoveFile {
            from: path("Foo"),
            to: path("bar"),
            base_digest: digest(1),
        },
        EditOperation::MoveFile {
            from: path("BAR"),
            to: path("foo"),
            base_digest: digest(2),
        },
    ];
    assert!(matches!(
        EditIr::new(revision(), alias_cycle, EditLimits::default()),
        Err(IrError::MoveCycle(_))
    ));

    let overlap = vec![
        EditOperation::MoveFile {
            from: path("source"),
            to: path("Target"),
            base_digest: digest(1),
        },
        add("target"),
    ];
    assert!(matches!(
        EditIr::new(revision(), overlap, EditLimits::default()),
        Err(IrError::PathConflict(_))
    ));
}

#[test]
fn lexical_paths_unsupported_formats_and_bounds_fail_closed_without_filesystem_access() {
    for malicious in [
        "../escape",
        "/absolute",
        "C:/windows",
        "a//b",
        "a\\b",
        "./a",
        "file:stream",
        "NUL",
        "con.txt",
        "COM1.rs",
        "COM¹",
        "lpt9",
        "LPT².log",
        "trailing.",
        "trailing ",
        "",
    ] {
        assert!(matches!(
            RootRelativePath::parse(malicious, 100),
            Err(IrError::InvalidPath(_))
        ));
    }
    assert!(RootRelativePath::parse("é.txt", 100).is_ok());
    assert!(RootRelativePath::parse("e\u{301}.txt", 100).is_ok());
    assert!(matches!(
        normalize(ModelEditFormat::ExactSearchReplace, b"anything", &context()),
        Err(NormalizeError::UnsupportedFormat(
            ModelEditFormat::ExactSearchReplace
        ))
    ));
    let limits = EditLimits {
        max_operations: 0,
        ..EditLimits::default()
    };
    assert!(matches!(
        EditIr::new(
            revision(),
            vec![EditOperation::AddFile {
                path: path("a"),
                content: text("x"),
                executable: false,
            }],
            limits
        ),
        Err(IrError::OperationLimit {
            actual: 1,
            limit: 0
        })
    ));
    let limits = EditLimits {
        max_content_bytes: 1,
        ..EditLimits::default()
    };
    assert!(matches!(
        EditIr::new(
            revision(),
            vec![EditOperation::AddFile {
                path: path("a"),
                content: text("xx"),
                executable: false,
            }],
            limits
        ),
        Err(IrError::ContentLimit {
            actual: 2,
            limit: 1
        })
    ));
    let context = NormalizationContext::new(
        revision(),
        EditLimits {
            max_input_bytes: 2,
            ..EditLimits::default()
        },
    );
    assert!(matches!(
        normalize(ModelEditFormat::StructuredJson, b"long", &context),
        Err(NormalizeError::InputLimit {
            actual: 4,
            limit: 2
        })
    ));

    let long_path = "a".repeat(5_000);
    let custom_limits = EditLimits {
        max_path_bytes: 6_000,
        ..EditLimits::default()
    };
    let long_ir = EditIr::new(
        revision(),
        vec![EditOperation::AddFile {
            path: RootRelativePath::parse(long_path.as_str(), custom_limits.max_path_bytes)
                .unwrap(),
            content: text("x"),
            executable: false,
        }],
        custom_limits,
    )
    .unwrap();
    assert_eq!(
        EditIr::from_canonical_bytes(&long_ir.canonical_bytes(), custom_limits).unwrap(),
        long_ir
    );
    let structured = serde_json::to_vec(&json!({
        "version": 1,
        "expected_revision": revision().to_string(),
        "operations": [{
            "op": "add_file",
            "path": long_path,
            "content": {"encoding": "utf8", "newline": "lf", "text": "x", "final_newline": false},
            "executable": false
        }]
    }))
    .unwrap();
    assert!(
        normalize_structured_json(
            &structured,
            &NormalizationContext::new(revision(), custom_limits)
        )
        .is_ok()
    );

    let bounded = NormalizationContext::new(
        revision(),
        EditLimits {
            max_content_bytes: 3,
            ..EditLimits::default()
        },
    );
    let escaped = format!(
        r#"{{"version":1,"expected_revision":"{}","operations":[{{"op":"add_file","path":"a","content":{{"encoding":"utf8","newline":"lf","text":"\u0061\u0062\u0063\u0064","final_newline":false}},"executable":false}}]}}"#,
        revision()
    );
    assert!(matches!(
        normalize_structured_json(escaped.as_bytes(), &bounded),
        Err(NormalizeError::Ir(IrError::ContentLimit { .. }))
    ));
}
