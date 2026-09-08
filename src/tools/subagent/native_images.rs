use super::*;

fn png() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn file_schema() -> Value {
    json!({"$ref": FILE_SCHEMA_REF})
}
fn nested_schema() -> Value {
    json!({"type":"object", "properties":{"result":file_schema(), "caption":{"type":"string"}}, "required":["result", "caption"], "additionalProperties":false})
}
fn real_file(root: &Path) -> Value {
    serde_json::to_value(
        FileStore::new(root)
            .import_bytes("parent", "test.png", "image/png", &png(), None)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn strict_file_schema_supports_root_and_fixed_required_paths() {
    let root = tempfile::tempdir().unwrap();
    let file = real_file(root.path());
    let contract = OutputContract::new(file_schema()).unwrap();
    assert_eq!(contract.bind("", file.clone()).unwrap(), file);
    assert!(contract.bind("{}", file.clone()).is_err());
    let contract = OutputContract::new(nested_schema()).unwrap();
    assert_eq!(
        contract.bind(r#"{"caption":"ok"}"#, file.clone()).unwrap()["result"],
        file
    );
    for text in ["", "not json", "[]", r#"{"result":null,"caption":"ok"}"#] {
        assert!(contract.bind(text, file.clone()).is_err(), "{text}");
    }
    let contract = OutputContract::new(json!({"type":"object", "properties":{"outer":{"type":"object", "properties":{"image":file_schema()}, "required":["image"]}}, "required":["outer"]})).unwrap();
    assert_eq!(
        contract.bind("", file.clone()).unwrap(),
        json!({"outer":{"image":file}})
    );
}

#[test]
fn strict_file_schema_rejects_ambiguous_optional_and_indirect_bindings() {
    for schema in [
        json!({"type":"array", "items":file_schema()}),
        json!({"anyOf":[file_schema(), {"type":"string"}]}),
        json!({"type":"object", "properties":{"result":file_schema()}}),
        json!({"type":"object", "properties":{"a":file_schema(), "b":file_schema()}, "required":["a","b"]}),
        json!({"$defs":{"image":file_schema()}, "$ref":"#/$defs/image"}),
        json!({"$ref":FILE_SCHEMA_REF, "description":"siblings unsupported"}),
    ] {
        assert!(OutputContract::new(schema.clone()).is_err(), "{schema}");
    }
    // Property names are not schema keywords.
    assert!(
        OutputContract::new(
            json!({"type":"object", "properties":{"items":file_schema()}, "required":["items"]})
        )
        .is_ok()
    );
}

fn native_manager(root: &Path, log: &Path) -> Subagents {
    let mut manager = manager_with_generic_harness(root, Vec::new());
    manager.config.harnesses =
        crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
            "generic".into(),
            crate::acp_child::AcpHarnessProfile {
                command: "python3".into(),
                args: vec![
                    format!(
                        "{}/src/tools/subagent/native-image-fixture.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    BASE64.encode(png()),
                    log.display().to_string(),
                ],
                permissions: Default::default(),
            },
        )]))
        .unwrap();
    manager
}

#[tokio::test]
async fn native_files_are_attached_bound_and_durably_published_across_roots() {
    let parent = tempfile::tempdir().unwrap();
    let child_root = tempfile::tempdir().unwrap();
    let log = parent.path().join("prompts.jsonl");
    let manager = native_manager(parent.path(), &log);
    let file = real_file(parent.path());
    let contract = OutputContract::for_request(
        Some(nested_schema()),
        vec![serde_json::from_value(file.clone()).unwrap(); 2],
        parent.path(),
        "parent",
    )
    .unwrap();
    let handle = manager
        .create(
            "native".into(),
            CreateOptions {
                cwd: Some(child_root.path().to_owned()),
                ..Default::default()
            },
            0,
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap();
    let request: Value = serde_json::from_str(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["prompt"].as_array().unwrap().len(), 2);
    assert_eq!(request["prompt"][1]["type"], "image");
    assert_eq!(
        BASE64
            .decode(request["prompt"][1]["data"].as_str().unwrap())
            .unwrap(),
        png()
    );
    assert_eq!(handle.output["caption"], "native");
    assert!(handle.updates.as_ref().is_none_or(|u| {
        !serde_json::to_string(u)
            .unwrap()
            .contains(&BASE64.encode(png()))
    }));
    let child_store = FileStore::new(child_root.path());
    assert!(
        !child_store
            .selected_parts(&handle.id, &file, None)
            .unwrap()
            .is_empty()
    );
    manager
        .close(&handle.id, &TurnCancellation::default())
        .await
        .unwrap();
    let store = FileStore::new(parent.path());
    assert!(
        !store
            .selected_parts("parent", &handle.output, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .selected_parts("sibling", &handle.output, None)
            .is_err()
    );
}

#[tokio::test]
async fn strict_native_errors_preserve_continuation_handle_ownership() {
    let root = tempfile::tempdir().unwrap();
    let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
    let contract =
        OutputContract::for_request(Some(file_schema()), Vec::new(), root.path(), "parent")
            .unwrap();
    let handle = manager
        .create(
            "root".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap();
    for prompt in ["zero", "multiple", "corrupt", "attempt"] {
        assert!(
            manager
                .prompt(
                    handle.clone(),
                    prompt.into(),
                    TurnCancellation::default(),
                    Some(&contract)
                )
                .await
                .is_err()
        );
        let state = manager.lookup(&handle).unwrap();
        let locked = state.lock().await;
        assert_eq!(locked.status, SubagentStatus::Idle);
        assert_eq!(locked.handle_generation, handle.generation);
        assert_eq!(locked.outcome, Some(GenerationOutcome::Failed));
    }
    let next = manager
        .prompt(
            handle.clone(),
            "root".into(),
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap();
    assert!(next.generation > handle.generation);
    assert!(
        manager
            .prompt(
                handle,
                "root".into(),
                TurnCancellation::default(),
                Some(&contract)
            )
            .await
            .is_err()
    );
    manager
        .close(&next.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn native_images_without_schema_have_an_explicit_descriptor_surface() {
    let root = tempfile::tempdir().unwrap();
    let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
    let contract = OutputContract::for_request(None, Vec::new(), root.path(), "parent").unwrap();
    let handle = manager
        .create(
            "native".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap();
    assert_eq!(handle.output["value"], r#"{"caption":"native"}"#);
    assert_eq!(handle.output["files"].as_array().unwrap().len(), 1);
    manager
        .close(&handle.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn attachment_authority_is_caller_scoped_and_failed_create_releases_capacity() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("prompts.jsonl");
    let manager = native_manager(root.path(), &log);
    let file = real_file(root.path());
    let contract = OutputContract::for_request(
        None,
        vec![serde_json::from_value(file).unwrap()],
        root.path(),
        "stranger",
    )
    .unwrap();
    let error = manager
        .create(
            "root".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("inaccessible"));
    assert!(!log.exists());
    wait_for_available_permits(&manager, MAX_LIVE_SUBAGENTS).await;
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn strict_fork_failure_does_not_consume_source_and_success_publishes_branch() {
    let root = tempfile::tempdir().unwrap();
    let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
    let contract = || {
        Arc::new(
            OutputContract::for_request(Some(file_schema()), Vec::new(), root.path(), "parent")
                .unwrap(),
        )
    };
    let handle = manager
        .create(
            "root".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            Some(&contract()),
        )
        .await
        .unwrap();
    assert!(
        manager
            .fork(
                handle.clone(),
                "zero".into(),
                None,
                0,
                TurnCancellation::default(),
                Some(contract())
            )
            .await
            .is_err()
    );
    let branch = manager
        .fork(
            handle.clone(),
            "root".into(),
            None,
            0,
            TurnCancellation::default(),
            Some(contract()),
        )
        .await
        .unwrap();
    assert_ne!(branch.id, handle.id);
    manager
        .close(&branch.id, &TurnCancellation::default())
        .await
        .unwrap();
    assert!(
        !FileStore::new(root.path())
            .selected_parts("parent", &branch.output, None)
            .unwrap()
            .is_empty()
    );
    let next = manager
        .prompt(
            handle,
            "root".into(),
            TurnCancellation::default(),
            Some(&contract()),
        )
        .await
        .unwrap();
    manager
        .close(&next.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_native_continuation_keeps_retry_handle() {
    let root = tempfile::tempdir().unwrap();
    let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
    let contract =
        OutputContract::for_request(Some(file_schema()), Vec::new(), root.path(), "parent")
            .unwrap();
    let handle = manager
        .create(
            "root".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap();
    let controller = agentkit_core::CancellationController::new();
    let cancel = controller.handle().checkpoint();
    controller.interrupt();
    assert!(
        manager
            .prompt(handle.clone(), "root".into(), cancel, Some(&contract))
            .await
            .is_err()
    );
    let next = manager
        .prompt(
            handle,
            "root".into(),
            TurnCancellation::default(),
            Some(&contract),
        )
        .await
        .unwrap();
    manager
        .close(&next.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[test]
fn all_subagent_input_schemas_accept_optional_typed_attachments() {
    let root = tempfile::tempdir().unwrap();
    let file = real_file(root.path());
    let manager = manager_with_generic_harness(root.path(), Vec::new());
    let create = SubagentTool::new(manager.clone(), 0);
    let continuation = PromptTool::new(manager.clone());
    let fork = ForkTool::new(manager, 0);
    for (schema, mut input) in [
        (&create.spec.input_schema, json!({"prompt":"work"})),
        (
            &continuation.spec.input_schema,
            json!({"prompt":"work","subagent":{"id":"s","output":null,"generation":1}}),
        ),
        (
            &fork.spec.input_schema,
            json!({"prompt":"work","subagent":{"id":"s","output":null,"generation":1}}),
        ),
    ] {
        let validator = jsonschema::validator_for(schema).unwrap();
        assert!(validator.is_valid(&input));
        input["attachments"] = json!([file]);
        assert!(validator.is_valid(&input));
        input["attachments"] = Value::Null;
        assert!(!validator.is_valid(&input));
        input["attachments"] = json!(["file_fabricated"]);
        assert!(!validator.is_valid(&input));
        input["attachments"] = Value::Array(vec![file.clone(); 9]);
        assert!(!validator.is_valid(&input));
    }
}

#[test]
fn legacy_schema_depth_and_width_are_not_subject_to_file_binding_limits() {
    let mut schema = json!({"type":"string"});
    let mut value = json!("leaf");
    for _ in 0..40 {
        schema = json!({"type":"object", "properties":{"child":schema}, "required":["child"]});
        value = json!({"child":value});
    }
    let contract = OutputContract::new(schema).unwrap();
    assert!(contract.binding.is_none());
    assert_eq!(
        contract.parse(&serde_json::to_string(&value).unwrap()),
        Some(value)
    );

    let properties = (0..10_001)
        .map(|index| (format!("field{index}"), json!({"type":"string"})))
        .collect::<Map<_, _>>();
    let contract = OutputContract::new(json!({"type":"object", "properties":properties})).unwrap();
    assert!(contract.binding.is_none());
    assert_eq!(contract.parse("{}"), Some(json!({})));
}

#[test]
fn literal_and_annotation_file_refs_do_not_activate_strict_binding() {
    for keyword in ["const", "default", "examples", "enum", "x-extension"] {
        let value = file_schema();
        let literal = if matches!(keyword, "examples" | "enum") {
            json!([value])
        } else {
            value.clone()
        };
        let contract = OutputContract::new(json!({keyword:literal})).unwrap();
        assert!(contract.binding.is_none(), "{keyword}");
        assert_eq!(
            contract.parse(&serde_json::to_string(&value).unwrap()),
            Some(value)
        );
    }
}

#[test]
fn deep_file_binding_still_has_a_strict_limit_even_after_deep_legacy_prefixes() {
    let mut schema = file_schema();
    for _ in 0..65 {
        schema = json!({"type":"object", "properties":{"child":schema}, "required":["child"]});
    }
    assert!(
        file_binding(&mut schema)
            .unwrap_err()
            .contains("traversal limits")
    );
    // A literal annotation is ignored even alongside the real binding.
    let mut schema = json!({"type":"object", "properties":{"image":file_schema()}, "required":["image"], "examples":[{"$ref":FILE_SCHEMA_REF}]});
    assert_eq!(
        file_binding(&mut schema).unwrap(),
        Some(vec!["image".into()])
    );
}

#[tokio::test]
async fn repeated_attachment_metadata_is_checked_before_deduplicating_grants() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("prompts.jsonl");
    let manager = native_manager(root.path(), &log);
    let original = real_file(root.path());
    let mut forged = original.clone();
    forged["name"] = json!("different.png");
    let contract = OutputContract::for_request(
        None,
        vec![
            serde_json::from_value(original).unwrap(),
            serde_json::from_value(forged).unwrap(),
        ],
        root.path(),
        "parent",
    )
    .unwrap();
    assert!(
        manager
            .create(
                "root".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                Some(&contract)
            )
            .await
            .is_err()
    );
    assert!(!log.exists());
    wait_for_available_permits(&manager, MAX_LIVE_SUBAGENTS).await;
}

#[tokio::test]
async fn byte_identical_native_duplicates_bind_once_and_preserve_surrounding_text() {
    let root = tempfile::tempdir().unwrap();
    let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
    for schema in [Some(nested_schema()), None] {
        let contract =
            OutputContract::for_request(schema.clone(), Vec::new(), root.path(), "parent").unwrap();
        let handle = manager
            .create(
                "duplicate".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                Some(&contract),
            )
            .await
            .unwrap();
        if schema.is_some() {
            assert_eq!(handle.output["caption"], "native");
            assert_eq!(handle.output["result"]["$kit"], "file");
        } else {
            assert_eq!(handle.output["value"], r#"{"caption":"native"}"#);
            assert_eq!(handle.output["files"].as_array().unwrap().len(), 1);
        }
        manager
            .close(&handle.id, &TurnCancellation::default())
            .await
            .unwrap();
        let parts = FileStore::new(root.path())
            .selected_parts("parent", &handle.output, None)
            .unwrap();
        assert_eq!(
            parts
                .iter()
                .filter(|p| matches!(p, agentkit_core::Part::Media(_)))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn every_native_occurrence_is_validated_and_byte_distinct_outputs_are_ambiguous() {
    let root = tempfile::tempdir().unwrap();
    let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
    let contract =
        OutputContract::for_request(Some(nested_schema()), Vec::new(), root.path(), "parent")
            .unwrap();
    for (prompt, expected) in [
        ("invalid-mime", "MIME"),
        ("invalid-png", "image format"),
        ("overcount", "budget"),
        ("same-pixels", "exactly one distinct"),
        ("multiple", "exactly one distinct"),
    ] {
        let error = manager
            .create(
                prompt.into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                Some(&contract),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{prompt}: {error}");
        wait_for_available_permits(&manager, MAX_LIVE_SUBAGENTS).await;
    }
}

#[test]
fn repeated_native_occurrences_do_not_evade_count_or_pixel_budgets() {
    let image = ImageContent::new(BASE64.encode(png()), "image/png");
    let error = distinct_native_images(&vec![image; 9], &TurnCancellation::default()).unwrap_err();
    assert!(error.to_string().contains("occurrence budget"));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(4096, 4096)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let image = ImageContent::new(BASE64.encode(bytes.into_inner()), "image/png");
    let error = distinct_native_images(&vec![image; 3], &TurnCancellation::default()).unwrap_err();
    assert!(error.to_string().contains("aggregate pixel budget"));
}

fn indexed_schema(index: Value, nested: bool) -> Value {
    let file = json!({"$ref":FILE_SCHEMA_REF, "x-kit-image-index":index});
    if nested {
        json!({"type":"object", "properties":{"result":file,"caption":{"type":"string"}}, "required":["result","caption"], "additionalProperties":false})
    } else {
        file
    }
}

#[test]
fn image_index_is_only_an_integer_annotation_on_the_exact_binding() {
    for index in [
        json!(true),
        json!("0"),
        json!(-1),
        json!(0.5),
        json!(8),
        Value::Null,
    ] {
        assert!(
            OutputContract::new(indexed_schema(index.clone(), false)).is_err(),
            "{index}"
        );
    }
    for index in [0, 7] {
        for nested in [false, true] {
            let contract = OutputContract::new(indexed_schema(json!(index), nested)).unwrap();
            assert_eq!(contract.image_index, Some(index));
        }
    }
    for schema in [
        json!({"type":"object", "x-kit-image-index":0}),
        json!({"$ref":FILE_SCHEMA_REF,"x-kit-image-index":0,"description":"not allowed"}),
        json!({"type":"object","properties":{"a":indexed_schema(json!(0),false),"b":file_schema()},"required":["a","b"]}),
        json!({"type":"array","items":indexed_schema(json!(0),false)}),
    ] {
        assert!(OutputContract::new(schema.clone()).is_err(), "{schema}");
    }
}

#[tokio::test]
async fn explicit_indices_promote_only_selected_exact_bytes_in_distinct_emission_order() {
    for (prompt, index, nested, occurrence) in [
        ("multiple", 0, true, 0),
        ("multiple", 1, true, 1),
        ("duplicate-distinct", 1, true, 2),
        ("multiple-root", 1, false, 1),
    ] {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("prompts.jsonl");
        let parent_session = session::new_id();
        let manager = native_manager(root.path(), &log);
        let contract = OutputContract::for_request(
            Some(indexed_schema(json!(index), nested)),
            Vec::new(),
            root.path(),
            &parent_session,
        )
        .unwrap();
        let handle = manager
            .create(
                prompt.into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                Some(&contract),
            )
            .await
            .unwrap();
        let emitted: Vec<Value> = std::fs::read_to_string(log.with_extension("jsonl.images"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let expected = BASE64
            .decode(emitted[occurrence]["data"].as_str().unwrap())
            .unwrap();
        let file: FileReference = serde_json::from_value(if nested {
            handle.output["result"].clone()
        } else {
            handle.output.clone()
        })
        .unwrap();
        let store = FileStore::new(root.path());
        assert_eq!(store.resolve(&parent_session, &file).unwrap(), expected);
        if nested {
            assert_eq!(handle.output["caption"], "native");
        }
        for session in [parent_session.as_str(), handle.id.as_str()] {
            let directory = crate::artifacts::base(root.path())
                .with_file_name("files")
                .join(blake3::hash(session.as_bytes()).to_hex().as_str());
            // Durable output objects, not internal execution instrumentation:
            // neither child nor parent receives any unselected snapshot.
            assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
        }
        assert!(
            handle
                .updates
                .as_ref()
                .is_none_or(|updates| !serde_json::to_string(updates).unwrap().contains("file_"))
        );
        manager
            .close(&handle.id, &TurnCancellation::default())
            .await
            .unwrap();
        assert_eq!(store.resolve(&parent_session, &file).unwrap(), expected);
    }
}

#[tokio::test]
async fn indexed_selection_never_bypasses_unselected_validation_or_missing_index_errors() {
    for (prompt, index, nested, expected) in [
        ("root", 1, false, "out of range"),
        ("duplicate-root", 1, false, "out of range"),
        ("invalid-mime", 0, true, "MIME"),
        ("invalid-png", 0, true, "image format"),
        ("overcount", 0, true, "budget"),
        ("overpixels", 0, true, "pixel budget"),
        ("attempt", 0, true, "binding field"),
        ("multiple", 0, false, "root File binding"),
        ("multiple-root", 0, true, "does not match output_schema"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let manager = native_manager(root.path(), &root.path().join("prompts.jsonl"));
        let contract = OutputContract::for_request(
            Some(indexed_schema(json!(index), nested)),
            Vec::new(),
            root.path(),
            "parent",
        )
        .unwrap();
        let error = manager
            .create(
                prompt.into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                Some(&contract),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{prompt}: {error}");
        wait_for_available_permits(&manager, MAX_LIVE_SUBAGENTS).await;
    }
}
