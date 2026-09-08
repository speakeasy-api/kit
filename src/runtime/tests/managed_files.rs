mod native_subagent;

use super::*;
use agentkit_core::{DataRef, Item, Modality, ToolResultPart};
use agentkit_http::Authentication;
use agentkit_loop::TurnRequest;
use agentkit_provider_openai::OpenAIResponsesConfig;
use base64::Engine as _;

struct Fixture {
    root: tempfile::TempDir,
    session: String,
    bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let session = format!(
            "managed-runtime-{}",
            blake3::hash(root.path().to_string_lossy().as_bytes()).to_hex()
        );
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        std::fs::write(root.path().join("image.png"), &bytes).unwrap();
        Self {
            root,
            session,
            bytes,
        }
    }

    async fn execute(
        &self,
        script: &str,
        input: Value,
        background: bool,
        outcome: bool,
    ) -> Result<ToolOutput, String> {
        // Reconstruct the runtime on every call to exercise restart-independent
        // storage rather than an in-memory resolver registry.
        let runtime = Runtime::new(self.root.path(), "gpt-5.4").unwrap();
        self.execute_with_runtime(runtime, script, input, background, outcome)
            .await
    }

    async fn execute_with_runtime(
        &self,
        runtime: Arc<Runtime>,
        script: &str,
        input: Value,
        background: bool,
        outcome: bool,
    ) -> Result<ToolOutput, String> {
        let compose = runtime.compose(0);
        assert_eq!(compose.specs().len(), 1);
        assert_eq!(compose.specs()[0].name.0, "compose");
        assert!(compose.specs()[0].description.contains("read_file"));
        let source: Arc<dyn ToolSource> = Arc::new(compose.compose.clone());
        let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([source]));
        let permissions = Arc::new(AllowAllPermissions);
        let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
        let session_id = SessionId::new(self.session.clone());
        let turn_id = TurnId::new("turn");
        let owned = OwnedToolContext {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            metadata: MetadataMap::new(),
            permissions: permissions.clone(),
            resources: resources.clone(),
            cancellation: None,
            execution_scope: Some(ToolExecutionScope {
                executor,
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                permissions,
                resources,
                cancellation: None,
            }),
            approved_request: None,
        };
        let request = ToolRequest::new(
            ToolCallId::new("image-call"),
            ToolName::new("compose"),
            json!({"script":script,"input":input,"background":background}),
            session_id,
            turn_id,
        );
        if outcome {
            match compose
                .backgroundable
                .invoke_outcome(request, &mut owned.borrowed())
                .await
            {
                ToolExecutionOutcome::Completed(result) => Ok(result.result.output),
                other => Err(format!("{other:?}")),
            }
        } else {
            compose
                .backgroundable
                .invoke(request, &mut owned.borrowed())
                .await
                .map(|result| result.result.output)
                .map_err(|error| error.to_string())
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = crate::artifacts::base(self.root.path());
        let directory = base
            .with_file_name("files")
            .join(blake3::hash(self.session.as_bytes()).to_hex().as_str());
        let _ = crate::resilient_fs::remove_dir_all(directory);
        let _ = crate::resilient_fs::remove_dir_all(crate::artifacts::session_directory(
            &base,
            &self.session,
        ));
    }
}

fn assert_image(output: &ToolOutput, bytes: &[u8]) {
    let ToolOutput::Parts(parts) = output else {
        panic!("expected selected image parts: {output:?}")
    };
    let images = parts
        .iter()
        .filter_map(|part| match part {
            Part::Media(media) if media.modality == Modality::Image => Some(media),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].data, DataRef::InlineBytes(bytes.to_vec()));
}

#[tokio::test]
async fn managed_files_compose_reaches_native_wire_and_survives_restart() {
    let fixture = Fixture::new();
    for (background, outcome) in [(false, false), (false, true), (true, false), (true, true)] {
        let output = fixture
            .execute(
                "return read_file({ path: \"image.png\" })",
                Value::Null,
                background,
                outcome,
            )
            .await
            .unwrap();
        assert_image(&output, &fixture.bytes);
        let request = TurnRequest {
            session_id: SessionId::new(fixture.session.clone()),
            turn_id: TurnId::new("wire"),
            transcript: vec![Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    "image-call",
                    output,
                ))],
            )],
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        };
        let config =
            OpenAIResponsesConfig::chatgpt_private("gpt-5.4", Authentication::bearer("test-key"));
        let wire = config.encode_request(&request).unwrap();
        let blocks = wire["input"][0]["output"].as_array().unwrap();
        assert!(blocks.iter().any(|block| block["type"] == "input_text"));
        let image = blocks
            .iter()
            .find(|block| block["type"] == "input_image")
            .unwrap();
        let expected = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&fixture.bytes)
        );
        assert_eq!(image["image_url"], expected);
    }
    let output = fixture
        .execute(
            "return read_file({ path: \"image.png\" })",
            Value::Null,
            false,
            false,
        )
        .await
        .unwrap();
    let ToolOutput::Parts(parts) = output else {
        panic!()
    };
    let Part::Structured(reference) = &parts[0] else {
        panic!()
    };
    std::fs::remove_file(fixture.root.path().join("image.png")).unwrap();
    let resumed = fixture
        .execute(
            "return { nested: [input, input] }",
            reference.value.clone(),
            true,
            true,
        )
        .await
        .unwrap();
    assert_image(&resumed, &fixture.bytes);
}

#[tokio::test]
async fn managed_files_select_only_returned_references_and_preserve_spilled_images() {
    let fixture = Fixture::new();
    let output = fixture
        .execute(
            "image = read_file({ path: \"image.png\" })\nreturn { ok: true }",
            Value::Null,
            false,
            true,
        )
        .await
        .unwrap();
    assert_eq!(output, ToolOutput::structured(json!({"ok":true})));
    let output = fixture.execute("image = read_file({ path: \"image.png\" })\nreturn { text: input, nested: [image, image] }", json!("large text ".repeat(3000)), true, true).await.unwrap();
    assert_image(&output, &fixture.bytes);
    let ToolOutput::Parts(parts) = output else {
        panic!()
    };
    let Part::Structured(spill) = &parts[0] else {
        panic!()
    };
    assert!(spill.value["artifact"].is_string());
    let text_bytes = serde_json::to_vec(&spill.value).unwrap().len()
        + parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.len()),
                _ => None,
            })
            .sum::<usize>();
    assert!(text_bytes <= 8192);
    assert!(matches!(&parts[1], Part::Text(text) if text.text.contains("/nested/0")));
    assert!(matches!(&parts[2], Part::Media(_)));
    let artifact = std::fs::read_to_string(spill.value["artifact"].as_str().unwrap()).unwrap();
    assert!(!artifact.contains("InlineBytes"));
    assert!(!artifact.contains("inline_bytes"));
}

#[tokio::test]
async fn managed_files_transform_pipeline_delivers_only_final_image_and_replays() {
    for (background, outcome) in [(false, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        let output = fixture.execute(
            "source = read_file({path: \"image.png\"})\nrotated = image_rotate({image: source, degrees: 90})\ncropped = image_crop({image: rotated, aspect_ratio: {width: 1, height: 1}, anchor: \"center\"})\nresized = image_resize({image: cropped, width: 4, height: 4, fit: \"contain\"})\nreceipt = export_file({file: resized, path: \"result.png\"})\nreturn {image: resized, receipt}",
            Value::Null, background, outcome,
        ).await.unwrap();
        let exported = std::fs::read(fixture.root.path().join("result.png")).unwrap();
        assert_image(&output, &exported);
        let image = image::load_from_memory(&exported).unwrap();
        assert_eq!((image.width(), image.height()), (4, 4));
        assert_eq!(
            std::fs::read(fixture.root.path().join("image.png")).unwrap(),
            fixture.bytes
        );
        let ToolOutput::Parts(parts) = output else {
            panic!()
        };
        let Part::Structured(result) = &parts[0] else {
            panic!()
        };
        let reference = result.value["image"].clone();
        std::fs::remove_file(fixture.root.path().join("image.png")).unwrap();
        std::fs::remove_file(fixture.root.path().join("result.png")).unwrap();
        let replay = fixture
            .execute("return input", reference, background, outcome)
            .await
            .unwrap();
        assert_image(&replay, &exported);
    }
}

#[tokio::test]
async fn managed_files_export_receipt_never_selects_pixels() {
    let fixture = Fixture::new();
    let output = fixture.execute(
        "source = read_file({path: \"image.png\"})\nrotated = image_rotate({image: source, degrees: 180})\nreturn export_file({file: rotated, path: \"receipt.png\"})",
        Value::Null, false, true,
    ).await.unwrap();
    assert_eq!(
        output,
        ToolOutput::structured(json!({
            "path":fixture.root.path().canonicalize().unwrap().join("receipt.png"),
            "size_bytes":std::fs::metadata(fixture.root.path().join("receipt.png")).unwrap().len(),
            "status":"exported"
        }))
    );
    assert!(fixture.root.path().join("receipt.png").is_file());
}

#[tokio::test]
async fn managed_files_unused_operations_still_execute_without_delivering_images() {
    let fixture = Fixture::new();
    let output = fixture.execute(
        "source = read_file({path: \"image.png\"})\nunused = export_file({file: source, path: \"unused.png\"})\nreturn {done: true}",
        Value::Null, false, true,
    ).await.unwrap();
    assert_eq!(output, ToolOutput::structured(json!({"done":true})));
    assert_eq!(
        std::fs::read(fixture.root.path().join("unused.png")).unwrap(),
        fixture.bytes
    );
    let error = fixture.execute(
        "source = read_file({path: \"image.png\"})\nunused = image_crop({image: source, aspect_ratio: {width: 8192, height: 1}, anchor: \"center\"})\nreturn {done: true}",
        Value::Null, false, true,
    ).await.unwrap_err();
    assert!(error.contains("nonzero"), "{error}");
}

#[tokio::test]
async fn managed_files_hidden_transform_schema_rejects_invalid_inputs() {
    let fixture = Fixture::new();
    for operation in [
        "image_rotate({image: source, degrees: 45})",
        "image_crop({image: source, aspect_ratio: {width: 0, height: 1}, anchor: \"center\"})",
        "image_crop({image: source, aspect_ratio: {width: 1, height: 1}, anchor: \"outside\"})",
        "image_resize({image: source, width: 2, height: 2, fit: \"unknown\"})",
        "image_resize({image: source, width: 2, height: 2, fit: \"contain\", extra: true})",
        "export_file({file: source, path: \"\"})",
    ] {
        let script = format!("source = read_file({{path: \"image.png\"}})\nreturn {operation}");
        assert!(
            fixture
                .execute(&script, Value::Null, false, true)
                .await
                .is_err(),
            "{operation}"
        );
    }
}

#[tokio::test]
async fn managed_files_delivery_failure_does_not_claim_rollback() {
    let fixture = Fixture::new();
    let error = fixture
        .execute(
            "_ = edit({ op: \"add\", path: \"effect.txt\", content: \"done\" })\nreturn input",
            json!({"$kit":"file","version":999}),
            false,
            true,
        )
        .await
        .unwrap_err();
    assert!(error.contains("compose program completed"));
    assert!(error.contains("do not rerun blindly"));
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("effect.txt")).unwrap(),
        "done"
    );
}
