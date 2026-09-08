use super::*;
use crate::acp_child::{AcpHarnessProfile, AcpHarnesses};
use std::collections::BTreeMap;

const PIPELINE: &str = r#"
source = read_file({ path: "image.png" })
rotated = image_rotate({ image: source, degrees: 90 })
cropped = image_crop({ image: rotated, aspect_ratio: { width: 1, height: 1 }, anchor: "center" })
child = subagent({
  prompt: input.prompt,
  model: input.model,
  attachments: [cropped],
  output_schema: input.schema
})
_ = close(child)
return child.output.result
"#;

fn output_schema(index: Option<u8>) -> Value {
    let mut binding = json!({"$ref":"kit://schemas/file/v1"});
    if let Some(index) = index {
        binding["x-kit-image-index"] = json!(index);
    }
    json!({
        "type":"object",
        "properties":{"result":binding},
        "required":["result"],
        "additionalProperties":false
    })
}

fn configured_runtime(
    fixture: &Fixture,
    name: &str,
    profile: AcpHarnessProfile,
    provider: crate::ProviderKind,
    model: &str,
) -> Arc<Runtime> {
    let runtime = Runtime::new_with_provider(fixture.root.path(), model, provider).unwrap();
    Runtime::with_acp_harnesses(
        runtime,
        AcpHarnesses::new(BTreeMap::from([(name.into(), profile)])).unwrap(),
        format!("acp.{name}"),
    )
    .unwrap()
}

fn delivered_image(output: &ToolOutput) -> &[u8] {
    let ToolOutput::Parts(parts) = output else {
        panic!("pipeline did not deliver native parts: {output:?}");
    };
    let images = parts
        .iter()
        .filter_map(|part| match part {
            Part::Media(media) if media.modality == Modality::Image => Some(media),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        images.len(),
        1,
        "only the final selected image is delivered"
    );
    let DataRef::InlineBytes(bytes) = &images[0].data else {
        panic!("native bytes required");
    };
    bytes
}

#[tokio::test]
async fn compose_transform_native_subagent_output_survives_close() {
    let fixture = Fixture::new();
    let log = fixture.root.path().join("wire.jsonl");
    let runtime = configured_runtime(
        &fixture,
        "image-fixture",
        AcpHarnessProfile {
            command: "python3".into(),
            args: vec![
                format!(
                    "{}/src/tools/subagent/native-image-fixture.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                base64::engine::general_purpose::STANDARD.encode(&fixture.bytes),
                log.display().to_string(),
            ],
            permissions: Default::default(),
        },
        crate::ProviderKind::OpenAiSubscription,
        "gpt-5.4",
    );
    let output = fixture
        .execute_with_runtime(
            runtime,
            PIPELINE,
            json!({"prompt":"root", "model":null, "schema":output_schema(None)}),
            false,
            true,
        )
        .await
        .unwrap();
    assert_eq!(delivered_image(&output), fixture.bytes);
    let request: Value = serde_json::from_str(
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["prompt"][1]["type"], "image");
    let attached = base64::engine::general_purpose::STANDARD
        .decode(request["prompt"][1]["data"].as_str().unwrap())
        .unwrap();
    let cropped = image::load_from_memory(&attached).unwrap();
    assert_eq!((cropped.width(), cropped.height()), (2, 2));
    assert_ne!(
        attached, fixture.bytes,
        "child receives the transformed snapshot"
    );
}

/// Explicit opt-in only: this performs a billable request to the selected provider.
/// Build this worktree's Kit binary, then set KIT_LIVE_KIT_BINARY and
/// KIT_LIVE_IMAGE_MODEL to an explicitly selected image-output model. The caller
/// explicitly selects distinct output index 0; the backend may emit alternatives.
#[tokio::test]
#[ignore = "requires an explicitly selected live image model and provider credentials"]
async fn live_compose_sticker_pipeline() {
    let binary =
        std::env::var("KIT_LIVE_KIT_BINARY").expect("set this worktree's built Kit binary");
    let model =
        std::env::var("KIT_LIVE_IMAGE_MODEL").expect("select a verified image-output model");
    let mut fixture = Fixture::new();
    let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        96,
        image::Rgb([153, 204, 255]),
    ));
    let mut encoded = std::io::Cursor::new(Vec::new());
    source
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    fixture.bytes = encoded.into_inner();
    std::fs::write(fixture.root.path().join("image.png"), &fixture.bytes).unwrap();
    let runtime = configured_runtime(
        &fixture,
        "kit",
        AcpHarnessProfile {
            command: binary,
            args: vec!["acp".into()],
            permissions: Default::default(),
        },
        crate::ProviderKind::OpenRouter,
        model
            .strip_prefix("openrouter:")
            .expect("select an OpenRouter image model"),
    );
    let output = fixture.execute_with_runtime(runtime, PIPELINE, json!({
        "model": model,
        "schema": output_schema(Some(0)),
        "prompt": "Add a small Hello Kitty sticker in the center of this blue image. Return exactly one edited image, no prose."
    }), false, true).await.unwrap();
    let bytes = delivered_image(&output);
    let image = image::load_from_memory(bytes).unwrap();
    assert!(image.width() > 0 && image.height() > 0);
    assert_ne!(
        bytes, fixture.bytes,
        "native generation must not echo the source bytes"
    );
    if let Ok(path) = std::env::var("KIT_LIVE_IMAGE_EVIDENCE") {
        std::fs::write(path, bytes).unwrap();
    }
}
