//! Hidden managed-image operations. Only compose finalization selects pixels.
use std::path::PathBuf;

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::read_file::{file_schema, object, object_schema, positive_integer};
use crate::managed_files::{Anchor, AspectRatio, FileReference, FileStore, Fit, Transform};

#[derive(Clone, Copy)]
enum Operation {
    Rotate,
    Crop,
    Resize,
    Export,
}

#[derive(Clone)]
pub struct ImageTool {
    root: PathBuf,
    store: FileStore,
    operation: Operation,
    spec: ToolSpec,
}

const TRANSFORM_POLICY: &str = " Consumes a session-authorized managed File, never a path. Normalizes all EXIF orientations before geometry and returns a new immutable RGBA8 PNG, stripping source metadata (not color-managed conversion). Nonanimated PNG/JPEG only. Limits: 8 MiB encoded, 8192 per dimension, 16 megapixels, 64 MiB decode, 128 MiB resize scratch and 256 MiB estimated live pixel work; otherwise-valid geometries can fail budgets. Cancellation is cooperative between stages; running codecs are not preempted. Only final returned references deliver pixels.";

impl ImageTool {
    pub fn rotate(root: PathBuf) -> Self {
        Self::new(
            root,
            Operation::Rotate,
            "image_rotate",
            "Rotate 90, 180, or 270 degrees clockwise.",
            object_schema([
                ("image", file_schema()),
                (
                    "degrees",
                    object([
                        ("type", Value::from("integer")),
                        (
                            "enum",
                            Value::Array(vec![Value::from(90), Value::from(180), Value::from(270)]),
                        ),
                    ]),
                ),
            ]),
        )
    }

    pub fn crop(root: PathBuf) -> Self {
        Self::new(
            root,
            Operation::Crop,
            "image_crop",
            "Take the largest inscribed crop with an integer-rounded aspect ratio. Floor the shortened dimension; zero fails. Anchor positions choose the retained region; centered odd remainders leave the extra pixel right/bottom.",
            object_schema([
                ("image", file_schema()),
                (
                    "aspect_ratio",
                    object_schema([
                        ("width", positive_integer(8192)),
                        ("height", positive_integer(8192)),
                    ]),
                ),
                (
                    "anchor",
                    object([
                        ("type", Value::from("string")),
                        (
                            "enum",
                            Value::Array(vec![
                                Value::from("center"),
                                Value::from("top_left"),
                                Value::from("top"),
                                Value::from("top_right"),
                                Value::from("left"),
                                Value::from("right"),
                                Value::from("bottom_left"),
                                Value::from("bottom"),
                                Value::from("bottom_right"),
                            ]),
                        ),
                    ]),
                ),
            ]),
        )
    }

    pub fn resize(root: PathBuf) -> Self {
        Self::new(
            root,
            Operation::Resize,
            "image_resize",
            "Resize with Triangle filtering; upscaling is allowed. contain fits within the width/height box, flooring the shortened dimension (zero fails), without padding. cover center-crops to the integer-rounded target aspect ratio then resizes exactly, permitting rounding distortion. stretch resizes exactly without preserving aspect ratio.",
            object_schema([
                ("image", file_schema()),
                ("width", positive_integer(8192)),
                ("height", positive_integer(8192)),
                (
                    "fit",
                    object([
                        ("type", Value::from("string")),
                        (
                            "enum",
                            Value::Array(vec![
                                Value::from("contain"),
                                Value::from("cover"),
                                Value::from("stretch"),
                            ]),
                        ),
                    ]),
                ),
            ]),
        )
    }

    pub fn export(root: PathBuf) -> Self {
        Self::new(
            root,
            Operation::Export,
            "export_file",
            "Export exact bytes of a session-authorized managed File to a NEW local file. Relative paths use the working directory; absolute paths and parent symlinks follow OS permissions, not a sandbox. Parent must exist. Atomic create-new refuses existing files, directories and final symlinks; never overwrites a source. Unix creation mode is 0600. Disk-only writes commit at successful file sync. Cancellation/error after creation intentionally retains potentially partial or complete output; retry refuses that existing destination. Returns a text-only path/status receipt, not a File reference.",
            object_schema([
                ("file", file_schema()),
                (
                    "path",
                    object([
                        ("type", Value::from("string")),
                        ("minLength", Value::from(1)),
                        ("maxLength", Value::from(4096)),
                    ]),
                ),
            ]),
        )
    }

    fn new(
        root: PathBuf,
        operation: Operation,
        name: &str,
        description: &str,
        schema: Value,
    ) -> Self {
        let export = matches!(operation, Operation::Export);
        let description = if export {
            description.to_owned()
        } else {
            format!("{description}{TRANSFORM_POLICY}")
        };
        // Deliberately effectful: transforms persist immutable objects and export
        // writes user-visible bytes. Unused calls still run inside compose.
        let output_schema = if export {
            object_schema([
                ("path", object([("type", Value::from("string"))])),
                ("size_bytes", positive_integer(8_388_608)),
                (
                    "status",
                    object([
                        ("type", Value::from("string")),
                        ("enum", Value::Array(vec![Value::from("exported")])),
                    ]),
                ),
            ])
        } else {
            file_schema()
        };
        Self {
            store: FileStore::new(&root),
            root,
            operation,
            spec: ToolSpec::new(ToolName::new(name), description, schema)
                .with_output_schema(output_schema),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RotateInput {
    image: FileReference,
    degrees: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CropInput {
    image: FileReference,
    aspect_ratio: AspectRatio,
    anchor: Anchor,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeInput {
    image: FileReference,
    width: u32,
    height: u32,
    fit: Fit,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportInput {
    file: FileReference,
    path: String,
}

enum Action {
    Transform(FileReference, Transform),
    Export(FileReference, PathBuf),
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|error| ToolError::InvalidInput(error.to_string()))
}

#[async_trait]
impl Tool for ImageTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let action = match self.operation {
            Operation::Rotate => {
                let input: RotateInput = parse(request.input)?;
                Action::Transform(
                    input.image,
                    Transform::Rotate {
                        degrees: input.degrees,
                    },
                )
            }
            Operation::Crop => {
                let input: CropInput = parse(request.input)?;
                Action::Transform(
                    input.image,
                    Transform::Crop {
                        aspect_ratio: input.aspect_ratio,
                        anchor: input.anchor,
                    },
                )
            }
            Operation::Resize => {
                let input: ResizeInput = parse(request.input)?;
                Action::Transform(
                    input.image,
                    Transform::Resize {
                        width: input.width,
                        height: input.height,
                        fit: input.fit,
                    },
                )
            }
            Operation::Export => {
                let input: ExportInput = parse(request.input)?;
                if input.path.is_empty() || input.path.len() > 4096 {
                    return Err(ToolError::InvalidInput(
                        "path must contain 1 to 4096 UTF-8 bytes".into(),
                    ));
                }
                Action::Export(input.file, self.root.join(input.path))
            }
        };
        let store = self.store.clone();
        let cancellation = context.cancellation.clone();
        let session = request.session_id.0;
        let value = tokio::task::spawn_blocking(move || match action {
            Action::Transform(image, transform) => {
                let reference =
                    store.transform(&session, &image, transform, cancellation.as_ref())?;
                serde_json::to_value(reference).map_err(|error| error.to_string())
            }
            Action::Export(file, path) => {
                let size_bytes = store.export(&session, &file, &path, cancellation.as_ref())?;
                Ok(object([
                    ("path", Value::from(path.to_string_lossy().into_owned())),
                    ("size_bytes", Value::from(size_bytes)),
                    ("status", Value::from("exported")),
                ]))
            }
        })
        .await
        .map_err(|error| ToolError::Internal(error.to_string()))?
        .map_err(ToolError::ExecutionFailed)?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(value),
        )))
    }
}
