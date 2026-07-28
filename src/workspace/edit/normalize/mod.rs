mod unified_diff;

use std::{collections::BTreeMap, fmt};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::events::ContentDigest;

use super::ir::{
    ByteRange, EDIT_IR_VERSION, EditIr, EditLimits, EditOperation, ExecutableMode, Newline,
    RevisionToken, RootRelativePath, TextContent, WireOperation, identity_key, preflight_json,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelEditFormat {
    WholeFile,
    UnifiedDiff,
    StructuredJson,
    SimplifiedContextPatch,
    ExactSearchReplace,
    LspWorkspaceEdit,
    AstGrep,
    NativeCodemod,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredEditWire {
    version: u32,
    expected_revision: RevisionToken,
    operations: Vec<WireOperation>,
}

pub(crate) fn structured_edit_schema(
    schema_id: &str,
    expected_revision: &RevisionToken,
    limits: EditLimits,
) -> Value {
    let content = json!({
        "additionalProperties": false,
        "properties": {
            "encoding": {"const": "utf8"},
            "final_newline": {"type": "boolean"},
            "newline": {"enum": ["lf", "crlf"]},
            "text": {"type": "string"}
        },
        "required": ["encoding", "newline", "text", "final_newline"],
        "type": "object"
    });
    let digest = json!({"pattern": "^(blake3|sha256):[0-9a-f]{64}$", "type": "string"});
    let path = json!({"minLength": 1, "type": "string"});
    json!({
        "$id": schema_id,
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "additionalProperties": false,
        "properties": {
            "expected_revision": {"const": expected_revision.as_str(), "type": "string"},
            "operations": {
                "items": {"oneOf": [
                    {
                        "additionalProperties": false,
                        "properties": {"content": content, "executable": {"type": "boolean"}, "op": {"const": "add_file"}, "path": path},
                        "required": ["op", "path", "content", "executable"], "type": "object"
                    },
                    {
                        "additionalProperties": false,
                        "properties": {"base_digest": digest, "op": {"const": "delete_file"}, "path": path},
                        "required": ["op", "path", "base_digest"], "type": "object"
                    },
                    {
                        "additionalProperties": false,
                        "properties": {"base_digest": digest, "from": path, "op": {"const": "move_file"}, "to": path},
                        "required": ["op", "from", "to", "base_digest"], "type": "object"
                    },
                    {
                        "additionalProperties": false,
                        "properties": {
                            "base_digest": digest,
                            "executable": {"enum": ["preserve", "executable", "non_executable"]},
                            "expected": content,
                            "op": {"const": "replace_range"},
                            "path": path,
                            "range": {"additionalProperties": false, "properties": {"end": {"minimum": 0, "type": "integer"}, "start": {"minimum": 0, "type": "integer"}}, "required": ["start", "end"], "type": "object"},
                            "replacement": content
                        },
                        "required": ["op", "path", "base_digest", "range", "expected", "replacement", "executable"], "type": "object"
                    }
                ]},
                "maxItems": limits.max_operations,
                "type": "array"
            },
            "version": {"const": EDIT_IR_VERSION}
        },
        "required": ["version", "expected_revision", "operations"],
        "type": "object"
    })
}

pub(crate) fn native_edit_schema() -> Value {
    let limits = EditLimits::default();
    let expected =
        RevisionToken::parse(format!("r:{}", "0".repeat(64))).expect("static revision is valid");
    let mut schema = structured_edit_schema("kit.native.edit.input.v1", &expected, limits);
    schema["properties"]["expected_revision"] = json!({
        "pattern": "^r:[0-9a-f]{64}$",
        "type": "string"
    });
    schema
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaseFile {
    content: TextContent,
    executable: bool,
    digest: ContentDigest,
}

impl BaseFile {
    pub fn new(bytes: &[u8], executable: bool) -> Result<Self, NormalizeError> {
        let content = TextContent::from_bytes(bytes)?;
        let digest = ContentDigest::parse(&format!("blake3:{}", blake3::hash(bytes).to_hex()))
            .expect("BLAKE3 formatting is valid");
        Ok(Self {
            content,
            executable,
            digest,
        })
    }

    pub fn content(&self) -> &TextContent {
        &self.content
    }

    pub fn executable(&self) -> bool {
        self.executable
    }

    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }
}

#[derive(Clone, Debug)]
pub struct NormalizationContext {
    expected_revision: RevisionToken,
    files: BTreeMap<String, (RootRelativePath, BaseFile)>,
    limits: EditLimits,
    default_newline: Newline,
}

impl NormalizationContext {
    pub fn new(expected_revision: RevisionToken, limits: EditLimits) -> Self {
        Self {
            expected_revision,
            files: BTreeMap::new(),
            limits,
            default_newline: Newline::Lf,
        }
    }

    pub fn with_default_newline(mut self, newline: Newline) -> Self {
        self.default_newline = newline;
        self
    }

    pub fn insert_file(
        &mut self,
        path: impl Into<String>,
        bytes: &[u8],
        executable: bool,
    ) -> Result<(), NormalizeError> {
        let path = RootRelativePath::parse(path, self.limits.max_path_bytes)?;
        let key = identity_key(&path, self.limits.identity_policy);
        if self.files.contains_key(&key) {
            return Err(NormalizeError::DuplicatePath(path.to_string()));
        }
        if bytes.len() > self.limits.max_content_bytes {
            return Err(super::ir::IrError::ContentLimit {
                actual: bytes.len(),
                limit: self.limits.max_content_bytes,
            }
            .into());
        }
        self.files
            .insert(key, (path, BaseFile::new(bytes, executable)?));
        Ok(())
    }

    pub fn expected_revision(&self) -> &RevisionToken {
        &self.expected_revision
    }

    pub fn limits(&self) -> EditLimits {
        self.limits
    }

    pub fn default_newline(&self) -> Newline {
        self.default_newline
    }

    pub fn file(&self, path: &RootRelativePath) -> Option<&BaseFile> {
        self.files
            .get(&identity_key(path, self.limits.identity_policy))
            .map(|(_, file)| file)
    }
}

pub fn normalize(
    format: ModelEditFormat,
    input: &[u8],
    context: &NormalizationContext,
) -> Result<EditIr, NormalizeError> {
    normalize_with_trace(format, input, context, &mut ())
}

pub fn normalize_with_trace(
    format: ModelEditFormat,
    input: &[u8],
    context: &NormalizationContext,
    trace: &mut impl super::EditTrace,
) -> Result<EditIr, NormalizeError> {
    if input.len() > context.limits.max_input_bytes {
        return Err(NormalizeError::InputLimit {
            actual: input.len(),
            limit: context.limits.max_input_bytes,
        });
    }
    trace.emit(super::EditTraceId::Normalize);
    let ir = match format {
        ModelEditFormat::WholeFile => normalize_whole_file(input, context),
        ModelEditFormat::UnifiedDiff => unified_diff::normalize(input, context),
        ModelEditFormat::StructuredJson => normalize_structured_json(input, context),
        unsupported => Err(NormalizeError::UnsupportedFormat(unsupported)),
    }?;
    trace.emit(super::EditTraceId::EditIrNew);
    Ok(ir)
}

pub fn normalize_whole_file(
    input: &[u8],
    context: &NormalizationContext,
) -> Result<EditIr, NormalizeError> {
    check_input_limit(input, context)?;
    preflight_json(input, context.limits)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        version: u32,
        expected_revision: RevisionToken,
        files: Vec<File>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        path: String,
        content: String,
        executable: bool,
    }

    let envelope: Envelope = serde_json::from_slice(input)
        .map_err(|error| NormalizeError::MalformedJson(error.to_string()))?;
    check_envelope(envelope.version, &envelope.expected_revision, context)?;
    let mut operations = Vec::new();
    operations
        .try_reserve_exact(envelope.files.len())
        .map_err(|_| super::ir::IrError::Allocation)?;
    let mut content_bytes = 0_usize;
    for file in envelope.files {
        let path = RootRelativePath::parse(file.path, context.limits.max_path_bytes)?;
        let replacement = TextContent::from_bytes(file.content.as_bytes())?;
        if let Some(base) = context.file(&path) {
            let executable = mode_change(base.executable, file.executable);
            content_bytes = content_bytes
                .checked_add(base.content.rendered_len())
                .and_then(|total| total.checked_add(replacement.rendered_len()))
                .ok_or(super::ir::IrError::Allocation)?;
            if content_bytes > context.limits.max_content_bytes {
                return Err(super::ir::IrError::ContentLimit {
                    actual: content_bytes,
                    limit: context.limits.max_content_bytes,
                }
                .into());
            }
            let expected = base.content.clone();
            operations.push(EditOperation::ReplaceRange {
                path,
                base_digest: base.digest.clone(),
                range: ByteRange::new(0, expected.rendered_len())?,
                expected,
                replacement,
                executable,
            });
        } else {
            content_bytes = content_bytes
                .checked_add(replacement.rendered_len())
                .ok_or(super::ir::IrError::Allocation)?;
            if content_bytes > context.limits.max_content_bytes {
                return Err(super::ir::IrError::ContentLimit {
                    actual: content_bytes,
                    limit: context.limits.max_content_bytes,
                }
                .into());
            }
            operations.push(EditOperation::AddFile {
                path,
                content: replacement,
                executable: file.executable,
            });
        }
    }
    EditIr::new(
        context.expected_revision.clone(),
        operations,
        context.limits,
    )
    .map_err(Into::into)
}

pub fn normalize_structured_json(
    input: &[u8],
    context: &NormalizationContext,
) -> Result<EditIr, NormalizeError> {
    check_input_limit(input, context)?;
    preflight_json(input, context.limits)?;
    let envelope: StructuredEditWire = serde_json::from_slice(input)
        .map_err(|error| NormalizeError::MalformedJson(error.to_string()))?;
    check_envelope(envelope.version, &envelope.expected_revision, context)?;
    let mut operations = Vec::new();
    operations
        .try_reserve_exact(envelope.operations.len())
        .map_err(|_| super::ir::IrError::Allocation)?;
    for operation in envelope.operations {
        operations.push(operation.into_operation(context.limits)?);
    }
    EditIr::new(
        context.expected_revision.clone(),
        operations,
        context.limits,
    )
    .map_err(Into::into)
}

fn check_envelope(
    version: u32,
    expected_revision: &RevisionToken,
    context: &NormalizationContext,
) -> Result<(), NormalizeError> {
    if version != super::ir::EDIT_IR_VERSION {
        return Err(NormalizeError::UnsupportedVersion(version));
    }
    if expected_revision != context.expected_revision() {
        return Err(NormalizeError::RevisionMismatch {
            expected: context.expected_revision().to_string(),
            supplied: expected_revision.to_string(),
        });
    }
    Ok(())
}

fn check_input_limit(input: &[u8], context: &NormalizationContext) -> Result<(), NormalizeError> {
    if input.len() > context.limits.max_input_bytes {
        return Err(NormalizeError::InputLimit {
            actual: input.len(),
            limit: context.limits.max_input_bytes,
        });
    }
    Ok(())
}

fn mode_change(old: bool, new: bool) -> ExecutableMode {
    match (old, new) {
        (false, true) => ExecutableMode::Executable,
        (true, false) => ExecutableMode::NonExecutable,
        _ => ExecutableMode::Preserve,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NormalizeError {
    UnsupportedFormat(ModelEditFormat),
    UnsupportedVersion(u32),
    InputLimit { actual: usize, limit: usize },
    MalformedJson(String),
    MalformedPatch { line: usize, reason: String },
    BinaryPatch,
    MissingBase(String),
    UnexpectedBase(String),
    BaseMismatch(String),
    DuplicatePath(String),
    RevisionMismatch { expected: String, supplied: String },
    UnsupportedPatch(String),
    Ir(super::ir::IrError),
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat(format) => {
                write!(formatter, "unsupported model edit format: {format:?}")
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported edit format version {version}")
            }
            Self::InputLimit { actual, limit } => {
                write!(formatter, "input bytes {actual} exceed limit {limit}")
            }
            Self::MalformedJson(reason) => {
                write!(formatter, "malformed structured edit JSON: {reason}")
            }
            Self::MalformedPatch { line, reason } => {
                write!(formatter, "malformed patch at line {line}: {reason}")
            }
            Self::BinaryPatch => formatter.write_str("binary patches are unsupported"),
            Self::MissingBase(path) => write!(formatter, "patch requires missing base file {path}"),
            Self::UnexpectedBase(path) => {
                write!(formatter, "add targets existing base file {path}")
            }
            Self::BaseMismatch(path) => {
                write!(formatter, "patch does not exactly match base file {path}")
            }
            Self::DuplicatePath(path) => write!(formatter, "duplicate path {path}"),
            Self::RevisionMismatch { expected, supplied } => {
                write!(
                    formatter,
                    "workspace revision mismatch: expected {expected}, supplied {supplied}"
                )
            }
            Self::UnsupportedPatch(reason) => write!(formatter, "unsupported patch: {reason}"),
            Self::Ir(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NormalizeError {}

impl From<super::ir::IrError> for NormalizeError {
    fn from(error: super::ir::IrError) -> Self {
        Self::Ir(error)
    }
}

pub(super) fn executable_mode(old: bool, new: bool) -> ExecutableMode {
    mode_change(old, new)
}
