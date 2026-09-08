use super::*;
use agentkit_core::{DataRef, Modality, ToolResultPart};
use agentkit_task_manager::{
    TaskKind, TaskLaunchRequest, TaskManager, TaskResolution, TaskStartContext, TaskStartOutcome,
    TurnTaskUpdate,
};
use agentkit_tools_core::{ToolContext, ToolError, ToolRegistry, ToolResult, ToolSpec};
use tokio::sync::{Notify, mpsc};

// A genuine external-tool boundary. The receiver owns incoming requests and the
// test owns the single release permit. notify_one retains that permit even when
// release precedes the wait. No lock or production instrumentation is involved.
struct UploadImage {
    spec: ToolSpec,
    requests: mpsc::UnboundedSender<Value>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl Tool for UploadImage {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.requests.send(request.input["file"].clone()).unwrap();
        let cancellation = context.cancellation.as_ref().expect("compose cancellation");
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ToolError::Cancelled),
            _ = self.release.notified() => {}
        }
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::Structured(json!({"accepted": true})),
        )))
    }
}

struct Fixture {
    root: tempfile::TempDir,
    session: String,
    bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let session = format!("managed-background-{}", uuid::Uuid::new_v4());
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        std::fs::write(root.path().join("image.png"), &bytes).unwrap();
        Self {
            root,
            session,
            bytes,
        }
    }

    async fn detached_completion(&self, input: Value, resumed: bool) -> Value {
        // Rebuild the complete compose source on each invocation. In particular,
        // resume cannot rely on a prior resolver object retaining the source.
        let (requests, mut received) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let children = ToolRegistry::new()
            .with(crate::tools::ReadFileTool::new(
                self.root.path().to_path_buf(),
            ))
            .with(UploadImage {
                spec: ToolSpec::new(
                    ToolName::new("upload_image"),
                    "Upload an image reference to an external service",
                    json!({"type":"object", "properties":{"file":{"type":"object"}},
                        "required":["file"], "additionalProperties":false}),
                )
                .with_output_schema(
                    json!({"type":"object", "properties":{"accepted":{"type":"boolean"}},
                    "required":["accepted"], "additionalProperties":false}),
                ),
                requests,
                release: release.clone(),
            });
        let inner = agentkit_tool_compose::ComposeTool::wrap(children.clone())
            .with_backend(super::super::HiddenRunletBackend(children.clone()));
        let compose = super::super::ComposeOnly {
            compose: inner.clone(),
            backgroundable: BackgroundableCompose::new(
                inner,
                BackgroundJobs::default(),
                self.root.path().to_path_buf(),
                children,
            ),
        };
        let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([
            Arc::new(compose) as Arc<dyn ToolSource>
        ]));
        let manager = super::super::background_task_manager();
        let tasks = manager.handle();
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        let session_id = SessionId::new(self.session.clone());
        let turn_id = TurnId::new(if resumed {
            "resumed-turn"
        } else {
            "originating-turn"
        });
        let call_id = ToolCallId::new(if resumed {
            "resumed-call"
        } else {
            "image-call"
        });
        let permissions = Arc::new(AllowAllPermissions);
        let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
        let context = OwnedToolContext {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            metadata: MetadataMap::new(),
            permissions: permissions.clone(),
            resources: resources.clone(),
            cancellation: Some(cancellation.clone()),
            execution_scope: Some(ToolExecutionScope {
                executor: executor.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                permissions,
                resources,
                cancellation: Some(cancellation.clone()),
            }),
            approved_request: None,
        };
        let script = if resumed {
            "receipt = upload_image({file: input})\nreturn {file: input, accepted: receipt.accepted}"
        } else {
            "file = read_file({path: \"image.png\"})\nreceipt = upload_image({file})\nreturn {file, accepted: receipt.accepted}"
        };
        let start = manager
            .start_task(
                TaskLaunchRequest::plain(
                    None,
                    ToolRequest::new(
                        call_id.clone(),
                        ToolName::new("compose"),
                        json!({"script":script, "input":input, "background":true}),
                        session_id,
                        turn_id.clone(),
                    ),
                ),
                TaskStartContext {
                    executor,
                    tool_context: context,
                },
            )
            .await
            .unwrap();
        let TaskStartOutcome::Pending {
            task_id,
            kind: TaskKind::Foreground,
        } = start
        else {
            panic!("expected the real foreground-then-detach route: {start:?}");
        };
        let reference = received.recv().await.expect("external upload request");
        assert_eq!(reference["$kit"], "file");
        let update = manager
            .wait_for_turn(&turn_id, Some(cancellation.clone()))
            .await
            .unwrap();
        let Some(TurnTaskUpdate::Detached(snapshot)) = update else {
            panic!("expected actual runner detach: {update:?}");
        };
        assert_eq!(snapshot.id, task_id);
        assert_eq!(snapshot.kind, TaskKind::Background);
        // The foreground runner is finished while the upload remains blocked.
        assert!(
            manager
                .wait_for_turn(&turn_id, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            manager
                .take_pending_loop_updates()
                .await
                .unwrap()
                .resolutions
                .is_empty()
        );
        if !resumed {
            std::fs::remove_file(self.root.path().join("image.png")).unwrap();
        }
        assert!(!self.root.path().join("image.png").exists());
        controller.interrupt();
        assert!(cancellation.is_cancelled());
        manager.on_turn_interrupted(&turn_id).await.unwrap();
        assert_eq!(tasks.list_running().await[0].kind, TaskKind::Background);
        release.notify_one();
        tasks.wait_for_idle().await;
        let mut updates = manager
            .take_pending_loop_updates()
            .await
            .unwrap()
            .resolutions;
        assert_eq!(
            updates.len(),
            1,
            "one deferred completion must reach the loop"
        );
        let TaskResolution::Item(item) = updates.pop_front().unwrap() else {
            panic!("expected completion item");
        };
        let Part::ToolResult(result) = &item.parts[0] else {
            panic!("expected tool result")
        };
        assert_eq!(result.call_id, call_id);
        let ToolOutput::Parts(parts) = &result.output else {
            panic!(
                "detached image was lost or stringified: {:?}",
                result.output
            );
        };
        let images: Vec<_> = parts
            .iter()
            .filter_map(|part| match part {
                Part::Media(media) if media.modality == Modality::Image => Some(media),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, DataRef::InlineBytes(self.bytes.clone()));
        assert!(parts.iter().any(|part| matches!(part, Part::Structured(_))));
        assert!(
            manager
                .take_pending_loop_updates()
                .await
                .unwrap()
                .resolutions
                .is_empty()
        );
        assert!(tasks.list_running().await.is_empty());
        assert_eq!(tasks.list_completed().await[0].id, task_id);
        reference
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = crate::artifacts::base(self.root.path());
        let _ = crate::resilient_fs::remove_dir_all(
            base.with_file_name("files")
                .join(blake3::hash(self.session.as_bytes()).to_hex().as_str()),
        );
        let _ = crate::resilient_fs::remove_dir_all(crate::artifacts::session_directory(
            &base,
            &self.session,
        ));
    }
}

#[tokio::test]
async fn managed_image_survives_runner_detach_turn_cancellation_and_resume() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = Fixture::new();
        let reference = fixture.detached_completion(Value::Null, false).await;
        // Cross a serialization boundary, as a stored descriptor does on resume.
        let restored = serde_json::from_str(&serde_json::to_string(&reference).unwrap()).unwrap();
        assert_eq!(fixture.detached_completion(restored, true).await, reference);
    })
    .await
    .expect("real background task runner did not deliver managed image");
}
