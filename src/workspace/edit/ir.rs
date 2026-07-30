use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::Error as _,
    ser::{SerializeSeq, SerializeStruct},
};
use unicode_normalization::UnicodeNormalization;

use crate::domain::events::ContentDigest;

pub const EDIT_IR_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditLimits {
    pub max_operations: usize,
    pub max_path_bytes: usize,
    pub max_content_bytes: usize,
    pub max_input_bytes: usize,
    pub max_authorization_entries: usize,
    pub max_authorization_name_bytes: usize,
    pub max_authorization_memory_bytes: usize,
    pub max_authorization_time: Duration,
    pub max_validation_read_bytes: usize,
    pub max_validation_memory_bytes: usize,
    pub max_validation_time: Duration,
    pub identity_policy: FilesystemIdentityPolicy,
}

impl Default for EditLimits {
    fn default() -> Self {
        Self {
            max_operations: 1_000,
            max_path_bytes: 4_096,
            max_content_bytes: 16 * 1024 * 1024,
            max_input_bytes: 64 * 1024 * 1024,
            max_authorization_entries: 100_000,
            max_authorization_name_bytes: 16 * 1024 * 1024,
            max_authorization_memory_bytes: 32 * 1024 * 1024,
            max_authorization_time: Duration::from_secs(2),
            max_validation_read_bytes: 64 * 1024 * 1024,
            max_validation_memory_bytes: 128 * 1024 * 1024,
            max_validation_time: Duration::from_secs(10),
            identity_policy: FilesystemIdentityPolicy::Portable,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemIdentityPolicy {
    Portable,
    /// Use only when the downstream workspace root is proven case-sensitive.
    CaseSensitive,
}

impl fmt::Display for FilesystemIdentityPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Portable => "portable",
            Self::CaseSensitive => "case_sensitive",
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RootRelativePath(String);

impl RootRelativePath {
    pub fn parse(value: impl Into<String>, max_bytes: usize) -> Result<Self, IrError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > max_bytes
            || value.starts_with('/')
            || value.starts_with('\\')
            || value.contains('\\')
            || value.contains('\0')
            || value.chars().any(|character| {
                character.is_control()
                    || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            })
            || value.split('/').any(|part| {
                part.is_empty()
                    || matches!(part, "." | "..")
                    || part.ends_with(['.', ' '])
                    || is_windows_reserved(part)
            })
            || value
                .split('/')
                .next()
                .is_some_and(|part| part.len() >= 2 && part.as_bytes()[1] == b':')
        {
            return Err(IrError::InvalidPath(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) const fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

fn is_windows_reserved(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

pub(crate) fn identity_key(path: &RootRelativePath, policy: FilesystemIdentityPolicy) -> String {
    let normalized: String = path.as_str().nfc().collect();
    match policy {
        FilesystemIdentityPolicy::Portable => normalized
            .chars()
            .flat_map(char::to_uppercase)
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .nfc()
            .collect(),
        FilesystemIdentityPolicy::CaseSensitive => normalized,
    }
}

impl fmt::Display for RootRelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RootRelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(
            String::deserialize(deserializer)?,
            EditLimits::default().max_path_bytes,
        )
        .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RevisionToken(String);

impl RevisionToken {
    pub fn parse(value: impl Into<String>) -> Result<Self, IrError> {
        let value = value.into();
        let valid = value.strip_prefix("r:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err(IrError::InvalidRevision(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RevisionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RevisionToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextEncoding {
    Utf8,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Newline {
    Lf,
    Crlf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TextContent {
    encoding: TextEncoding,
    newline: Newline,
    text: String,
    final_newline: bool,
}

impl TextContent {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IrError> {
        let value = std::str::from_utf8(bytes).map_err(|_| IrError::UnsupportedEncoding)?;
        if value.contains('\0') {
            return Err(IrError::BinaryContent);
        }
        let has_crlf = value.contains("\r\n");
        let mut normalized = String::new();
        normalized
            .try_reserve_exact(value.len())
            .map_err(|_| IrError::Allocation)?;
        let mut chars = value.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\r' {
                if chars.next_if_eq(&'\n').is_none() {
                    return Err(IrError::InvalidNewline);
                }
                normalized.push('\n');
            } else {
                if has_crlf && character == '\n' {
                    return Err(IrError::InvalidNewline);
                }
                normalized.push(character);
            }
        }
        let final_newline = normalized.ends_with('\n');
        if final_newline {
            normalized.pop();
        }
        Self::new(
            normalized,
            if has_crlf { Newline::Crlf } else { Newline::Lf },
            final_newline,
        )
    }

    pub fn new(text: String, newline: Newline, final_newline: bool) -> Result<Self, IrError> {
        if text.contains('\r') || (!final_newline && text.ends_with('\n')) {
            return Err(IrError::InvalidTextContent);
        }
        if text.contains('\0') {
            return Err(IrError::BinaryContent);
        }
        Ok(Self {
            encoding: TextEncoding::Utf8,
            newline,
            text,
            final_newline,
        })
    }

    pub fn empty(newline: Newline) -> Self {
        Self {
            encoding: TextEncoding::Utf8,
            newline,
            text: String::new(),
            final_newline: false,
        }
    }

    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    pub fn newline(&self) -> Newline {
        self.newline
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn has_final_newline(&self) -> bool {
        self.final_newline
    }

    pub fn render(&self) -> Vec<u8> {
        self.try_render(usize::MAX)
            .expect("validated text can be rendered")
    }

    pub(crate) fn try_render(&self, max_bytes: usize) -> Result<Vec<u8>, IrError> {
        let separator = match self.newline {
            Newline::Lf => b"\n".as_slice(),
            Newline::Crlf => b"\r\n".as_slice(),
        };
        let rendered_len = self.rendered_len();
        if rendered_len > max_bytes {
            return Err(IrError::ContentLimit {
                actual: rendered_len,
                limit: max_bytes,
            });
        }
        let mut rendered = Vec::new();
        rendered
            .try_reserve_exact(rendered_len)
            .map_err(|_| IrError::Allocation)?;
        for (index, part) in self.text.split('\n').enumerate() {
            if index != 0 {
                rendered.extend_from_slice(separator);
            }
            rendered.extend_from_slice(part.as_bytes());
        }
        if self.final_newline {
            rendered.extend_from_slice(separator);
        }
        Ok(rendered)
    }

    pub fn rendered_len(&self) -> usize {
        let newlines = self.text.bytes().filter(|byte| *byte == b'\n').count()
            + usize::from(self.final_newline);
        self.text.len()
            + usize::from(self.final_newline)
            + newlines * usize::from(self.newline == Newline::Crlf)
    }
}

impl<'de> Deserialize<'de> for TextContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            encoding: TextEncoding,
            newline: Newline,
            text: String,
            final_newline: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.encoding != TextEncoding::Utf8 {
            return Err(D::Error::custom("unsupported text encoding"));
        }
        Self::new(wire.text, wire.newline, wire.final_newline).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableMode {
    Preserve,
    Executable,
    NonExecutable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: usize, end: usize) -> Result<Self, IrError> {
        if start > end {
            return Err(IrError::InvalidRange { start, end });
        }
        Ok(Self {
            start: start as u64,
            end: end as u64,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditOperation {
    AddFile {
        path: RootRelativePath,
        content: TextContent,
        executable: bool,
    },
    DeleteFile {
        path: RootRelativePath,
        base_digest: ContentDigest,
    },
    MoveFile {
        from: RootRelativePath,
        to: RootRelativePath,
        base_digest: ContentDigest,
    },
    ReplaceRange {
        path: RootRelativePath,
        base_digest: ContentDigest,
        range: ByteRange,
        expected: TextContent,
        replacement: TextContent,
        executable: ExecutableMode,
    },
}

impl EditOperation {
    pub fn primary_path(&self) -> &RootRelativePath {
        match self {
            Self::AddFile { path, .. }
            | Self::DeleteFile { path, .. }
            | Self::ReplaceRange { path, .. } => path,
            Self::MoveFile { from, .. } => from,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CanonicalOperation {
    id: String,
    order: u32,
    #[serde(flatten)]
    operation: EditOperation,
}

impl CanonicalOperation {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn order(&self) -> u32 {
        self.order
    }

    pub fn operation(&self) -> &EditOperation {
        &self.operation
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EditIr {
    version: u32,
    identity_policy: FilesystemIdentityPolicy,
    expected_revision: RevisionToken,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_change_diff_digest: Option<String>,
    operations: Vec<CanonicalOperation>,
}

impl EditIr {
    pub fn new(
        expected_revision: RevisionToken,
        operations: Vec<EditOperation>,
        limits: EditLimits,
    ) -> Result<Self, IrError> {
        if operations.len() > limits.max_operations {
            return Err(IrError::OperationLimit {
                actual: operations.len(),
                limit: limits.max_operations,
            });
        }
        validate_operations(&operations, limits)?;
        let mut canonical = Vec::new();
        canonical
            .try_reserve_exact(operations.len())
            .map_err(|_| IrError::Allocation)?;
        for (index, operation) in operations.into_iter().enumerate() {
            let order = u32::try_from(index).map_err(|_| IrError::OperationLimit {
                actual: index + 1,
                limit: u32::MAX as usize,
            })?;
            let encoded =
                serde_json::to_vec(&(EDIT_IR_VERSION, limits.identity_policy, order, &operation))
                    .expect("edit operation serialization is infallible");
            canonical.push(CanonicalOperation {
                id: format!("op:{order:08x}:{}", blake3::hash(&encoded).to_hex()),
                order,
                operation,
            });
        }
        Ok(Self {
            version: EDIT_IR_VERSION,
            identity_policy: limits.identity_policy,
            expected_revision,
            expected_change_diff_digest: None,
            operations: canonical,
        })
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn expected_revision(&self) -> &RevisionToken {
        &self.expected_revision
    }

    pub fn identity_policy(&self) -> FilesystemIdentityPolicy {
        self.identity_policy
    }

    pub fn expected_change_diff_digest(&self) -> Option<&str> {
        self.expected_change_diff_digest.as_deref()
    }

    pub fn with_expected_change_diff_digest(mut self, digest: String) -> Result<Self, IrError> {
        if !valid_change_diff_digest(&digest) {
            return Err(IrError::InvalidChangeDiffDigest);
        }
        self.expected_change_diff_digest = Some(digest);
        Ok(self)
    }

    pub fn operations(&self) -> &[CanonicalOperation] {
        &self.operations
    }

    pub fn apply_payload(&self) -> EditApplyPayload<'_> {
        EditApplyPayload(self)
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("canonical edit IR serialization is infallible")
    }

    pub fn digest(&self) -> String {
        format!("blake3:{}", blake3::hash(&self.canonical_bytes()).to_hex())
    }

    pub fn from_canonical_bytes(bytes: &[u8], limits: EditLimits) -> Result<Self, IrError> {
        if bytes.len() > limits.max_input_bytes {
            return Err(IrError::InputLimit {
                actual: bytes.len(),
                limit: limits.max_input_bytes,
            });
        }
        preflight_json(bytes, limits)?;
        let parsed: WireEditIr = serde_json::from_slice(bytes)
            .map_err(|error| IrError::MalformedIr(error.to_string()))?;
        if parsed.version != EDIT_IR_VERSION {
            return Err(IrError::UnsupportedVersion(parsed.version));
        }
        if parsed.identity_policy != limits.identity_policy {
            return Err(IrError::IdentityPolicyMismatch {
                expected: limits.identity_policy,
                supplied: parsed.identity_policy,
            });
        }
        if parsed.operations.len() > limits.max_operations {
            return Err(IrError::OperationLimit {
                actual: parsed.operations.len(),
                limit: limits.max_operations,
            });
        }
        let mut supplied_metadata = Vec::new();
        supplied_metadata
            .try_reserve_exact(parsed.operations.len())
            .map_err(|_| IrError::Allocation)?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(parsed.operations.len())
            .map_err(|_| IrError::Allocation)?;
        for operation in parsed.operations {
            supplied_metadata.push((operation.id, operation.order));
            operations.push(operation.operation.into_operation(limits)?);
        }
        let mut rebuilt = Self::new(parsed.expected_revision, operations, limits)?;
        if let Some(digest) = parsed.expected_change_diff_digest {
            rebuilt = rebuilt.with_expected_change_diff_digest(digest)?;
        }
        if supplied_metadata
            .iter()
            .zip(&rebuilt.operations)
            .any(|((id, order), rebuilt)| id != rebuilt.id() || *order != rebuilt.order())
        {
            return Err(IrError::NonCanonical);
        }
        if rebuilt.canonical_bytes() != bytes {
            return Err(IrError::NonCanonical);
        }
        Ok(rebuilt)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEditIr {
    version: u32,
    identity_policy: FilesystemIdentityPolicy,
    expected_revision: RevisionToken,
    #[serde(default)]
    expected_change_diff_digest: Option<String>,
    operations: Vec<WireCanonicalOperation>,
}

pub struct EditApplyPayload<'a>(&'a EditIr);

impl Serialize for EditApplyPayload<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let ir = self.0;
        let mut state = serializer.serialize_struct(
            "EditApplyPayload",
            3 + usize::from(ir.expected_change_diff_digest.is_some()),
        )?;
        state.serialize_field("version", &ir.version)?;
        state.serialize_field("expected_revision", &ir.expected_revision)?;
        if let Some(digest) = &ir.expected_change_diff_digest {
            state.serialize_field("expected_change_diff_digest", digest)?;
        }
        state.serialize_field("operations", &ApplyOperations(&ir.operations))?;
        state.end()
    }
}

struct ApplyOperations<'a>(&'a [CanonicalOperation]);

impl Serialize for ApplyOperations<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for operation in self.0 {
            sequence.serialize_element(operation.operation())?;
        }
        sequence.end()
    }
}

fn valid_change_diff_digest(value: &str) -> bool {
    value.strip_prefix("blake3:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[derive(Deserialize)]
struct WireCanonicalOperation {
    id: String,
    order: u32,
    #[serde(flatten)]
    operation: WireOperation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireTextContent {
    encoding: TextEncoding,
    newline: Newline,
    text: String,
    final_newline: bool,
}

impl WireTextContent {
    fn into_content(self) -> Result<TextContent, IrError> {
        if self.encoding != TextEncoding::Utf8 {
            return Err(IrError::UnsupportedEncoding);
        }
        TextContent::new(self.text, self.newline, self.final_newline)
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WireOperation {
    AddFile {
        path: String,
        content: WireTextContent,
        executable: bool,
    },
    DeleteFile {
        path: String,
        base_digest: ContentDigest,
    },
    MoveFile {
        from: String,
        to: String,
        base_digest: ContentDigest,
    },
    ReplaceRange {
        path: String,
        base_digest: ContentDigest,
        range: ByteRange,
        expected: WireTextContent,
        replacement: WireTextContent,
        executable: ExecutableMode,
    },
}

impl WireOperation {
    pub(super) fn into_operation(self, limits: EditLimits) -> Result<EditOperation, IrError> {
        Ok(match self {
            Self::AddFile {
                path,
                content,
                executable,
            } => EditOperation::AddFile {
                path: RootRelativePath::parse(path, limits.max_path_bytes)?,
                content: content.into_content()?,
                executable,
            },
            Self::DeleteFile { path, base_digest } => EditOperation::DeleteFile {
                path: RootRelativePath::parse(path, limits.max_path_bytes)?,
                base_digest,
            },
            Self::MoveFile {
                from,
                to,
                base_digest,
            } => EditOperation::MoveFile {
                from: RootRelativePath::parse(from, limits.max_path_bytes)?,
                to: RootRelativePath::parse(to, limits.max_path_bytes)?,
                base_digest,
            },
            Self::ReplaceRange {
                path,
                base_digest,
                range,
                expected,
                replacement,
                executable,
            } => EditOperation::ReplaceRange {
                path: RootRelativePath::parse(path, limits.max_path_bytes)?,
                base_digest,
                range,
                expected: expected.into_content()?,
                replacement: replacement.into_content()?,
                executable,
            },
        })
    }
}

pub(crate) fn preflight_json(bytes: &[u8], limits: EditLimits) -> Result<(), IrError> {
    let mut index = 0;
    let mut operations = 0_usize;
    let mut paths = 0_usize;
    let mut content_bytes = 0_usize;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let (end, _) = json_string(bytes, index)?;
        let key = &bytes[index + 1..end - 1];
        let is_op = json_key_eq(key, b"op");
        let is_path = json_key_eq(key, b"path");
        let is_from = json_key_eq(key, b"from");
        let is_to = json_key_eq(key, b"to");
        let is_text = json_key_eq(key, b"text");
        let is_content = json_key_eq(key, b"content");
        let mut next = end;
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        if bytes.get(next) != Some(&b':') {
            index = end;
            continue;
        }
        if is_op {
            operations = operations.checked_add(1).ok_or(IrError::Allocation)?;
            if operations > limits.max_operations {
                return Err(IrError::OperationLimit {
                    actual: operations,
                    limit: limits.max_operations,
                });
            }
        }
        if is_path {
            paths = paths.checked_add(1).ok_or(IrError::Allocation)?;
        }
        next += 1;
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        if bytes.get(next) == Some(&b'"') {
            let (_, decoded) = json_string(bytes, next)?;
            let limit = (is_path || is_from || is_to)
                .then_some(limits.max_path_bytes)
                .or_else(|| (is_text || is_content).then_some(limits.max_content_bytes));
            if let Some(limit) = limit
                && decoded > limit
            {
                return Err(if is_path || is_from || is_to {
                    IrError::InvalidPath("path exceeds active byte limit".to_owned())
                } else {
                    IrError::ContentLimit {
                        actual: decoded,
                        limit,
                    }
                });
            }
            if is_text || is_content {
                content_bytes = content_bytes
                    .checked_add(decoded)
                    .ok_or(IrError::Allocation)?;
                if content_bytes > limits.max_content_bytes {
                    return Err(IrError::ContentLimit {
                        actual: content_bytes,
                        limit: limits.max_content_bytes,
                    });
                }
            }
        }
        index = end;
    }
    if operations == 0 && paths > limits.max_operations {
        return Err(IrError::OperationLimit {
            actual: paths,
            limit: limits.max_operations,
        });
    }
    Ok(())
}

fn json_key_eq(raw: &[u8], expected: &[u8]) -> bool {
    let mut raw_index = 0;
    let mut expected_index = 0;
    while raw_index < raw.len() && expected_index < expected.len() {
        let (byte, consumed) = if raw[raw_index] == b'\\' {
            if raw.get(raw_index + 1) == Some(&b'u') {
                let Some(digits) = raw.get(raw_index + 2..raw_index + 6) else {
                    return false;
                };
                let Some(value) = std::str::from_utf8(digits)
                    .ok()
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                else {
                    return false;
                };
                (value, 6)
            } else {
                let Some(&escaped) = raw.get(raw_index + 1) else {
                    return false;
                };
                (escaped, 2)
            }
        } else {
            (raw[raw_index], 1)
        };
        if byte != expected[expected_index] {
            return false;
        }
        raw_index += consumed;
        expected_index += 1;
    }
    raw_index == raw.len() && expected_index == expected.len()
}

fn json_string(bytes: &[u8], start: usize) -> Result<(usize, usize), IrError> {
    let mut index = start + 1;
    let mut decoded = 0_usize;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' => return Ok((index + 1, decoded)),
            b'\\' => {
                let escaped = *bytes
                    .get(index + 1)
                    .ok_or_else(|| IrError::MalformedIr("unterminated JSON escape".to_owned()))?;
                if escaped == b'u' {
                    let digits = bytes.get(index + 2..index + 6).ok_or_else(|| {
                        IrError::MalformedIr("unterminated JSON unicode escape".to_owned())
                    })?;
                    let value = std::str::from_utf8(digits)
                        .ok()
                        .and_then(|digits| u16::from_str_radix(digits, 16).ok())
                        .ok_or_else(|| {
                            IrError::MalformedIr("invalid JSON unicode escape".to_owned())
                        })?;
                    if (0xd800..=0xdbff).contains(&value)
                        && bytes.get(index + 6..index + 8) == Some(b"\\u")
                    {
                        let low = bytes
                            .get(index + 8..index + 12)
                            .and_then(|digits| std::str::from_utf8(digits).ok())
                            .and_then(|digits| u16::from_str_radix(digits, 16).ok());
                        if let Some(low @ 0xdc00..=0xdfff) = low {
                            let scalar = 0x1_0000
                                + ((u32::from(value) - 0xd800) << 10)
                                + (u32::from(low) - 0xdc00);
                            decoded = decoded
                                .checked_add(char::from_u32(scalar).map_or(4, char::len_utf8))
                                .ok_or(IrError::Allocation)?;
                            index += 12;
                            continue;
                        }
                    }
                    let scalar = char::from_u32(u32::from(value)).ok_or_else(|| {
                        IrError::MalformedIr("invalid JSON unicode surrogate".to_owned())
                    })?;
                    decoded = decoded
                        .checked_add(scalar.len_utf8())
                        .ok_or(IrError::Allocation)?;
                    index += 6;
                } else {
                    decoded = decoded.checked_add(1).ok_or(IrError::Allocation)?;
                    index += 2;
                }
            }
            _ => {
                decoded = decoded.checked_add(1).ok_or(IrError::Allocation)?;
                index += 1;
            }
        }
    }
    Err(IrError::MalformedIr("unterminated JSON string".to_owned()))
}

type RangesByIdentity<'a> =
    BTreeMap<String, (&'a RootRelativePath, Vec<(ByteRange, &'a ContentDigest)>)>;

fn validate_operations(operations: &[EditOperation], limits: EditLimits) -> Result<(), IrError> {
    let mut paths = RangesByIdentity::new();
    let mut exclusive = BTreeMap::new();
    let mut moves = BTreeMap::new();
    let mut executable_modes = BTreeMap::new();
    let mut content_bytes = 0_usize;

    for operation in operations {
        for path in operation_paths(operation) {
            if path.as_str().len() > limits.max_path_bytes {
                return Err(IrError::InvalidPath(path.to_string()));
            }
        }
        match operation {
            EditOperation::AddFile { path, content, .. } => {
                claim_exclusive(path, limits.identity_policy, &mut exclusive, &paths)?;
                content_bytes = content_bytes
                    .checked_add(content.rendered_len())
                    .ok_or(IrError::Allocation)?;
            }
            EditOperation::DeleteFile { path, .. } => {
                claim_exclusive(path, limits.identity_policy, &mut exclusive, &paths)?;
            }
            EditOperation::MoveFile { from, to, .. } => {
                let from_key = identity_key(from, limits.identity_policy);
                let to_key = identity_key(to, limits.identity_policy);
                if from_key == to_key {
                    return Err(IrError::PathConflict(from.to_string()));
                }
                if moves.insert(from_key, (to_key, from, to)).is_some() {
                    return Err(IrError::PathConflict(from.to_string()));
                }
            }
            EditOperation::ReplaceRange {
                path,
                base_digest,
                range,
                expected,
                replacement,
                executable,
            } => {
                if range.start > range.end
                    || range.end - range.start != expected.rendered_len() as u64
                {
                    return Err(IrError::InvalidRange {
                        start: range.start as usize,
                        end: range.end as usize,
                    });
                }
                let key = identity_key(path, limits.identity_policy);
                if exclusive.contains_key(&key) {
                    return Err(IrError::PathConflict(path.to_string()));
                }
                let (original, ranges) = paths.entry(key.clone()).or_insert((path, Vec::new()));
                if *original != path {
                    return Err(IrError::PathConflict(path.to_string()));
                }
                ranges.push((*range, base_digest));
                if let Some(previous) = executable_modes.insert(key, *executable)
                    && previous != *executable
                {
                    return Err(IrError::ExecutableModeConflict(path.to_string()));
                }
                content_bytes = content_bytes
                    .checked_add(expected.rendered_len())
                    .and_then(|total| total.checked_add(replacement.rendered_len()))
                    .ok_or(IrError::Allocation)?;
            }
        }
        if content_bytes > limits.max_content_bytes {
            return Err(IrError::ContentLimit {
                actual: content_bytes,
                limit: limits.max_content_bytes,
            });
        }
    }

    detect_move_cycles(&moves)?;
    for (_, (_, from, to)) in moves {
        claim_exclusive(from, limits.identity_policy, &mut exclusive, &paths)?;
        claim_exclusive(to, limits.identity_policy, &mut exclusive, &paths)?;
    }
    for (path, ranges) in paths.values_mut() {
        ranges.sort_by_key(|(range, _)| (range.start, range.end));
        let digest = ranges[0].1;
        if ranges.iter().any(|(_, candidate)| *candidate != digest) {
            return Err(IrError::BaseDigestConflict(path.to_string()));
        }
        for pair in ranges.windows(2) {
            let previous = pair[0].0;
            let current = pair[1].0;
            if current.start < previous.end
                || (current.start == previous.start
                    && (current.start == current.end || previous.start == previous.end))
            {
                return Err(IrError::OverlappingRanges(path.to_string()));
            }
        }
    }
    Ok(())
}

fn operation_paths(operation: &EditOperation) -> [&RootRelativePath; 2] {
    match operation {
        EditOperation::MoveFile { from, to, .. } => [from, to],
        operation => [operation.primary_path(), operation.primary_path()],
    }
}

fn claim_exclusive<'a>(
    path: &'a RootRelativePath,
    policy: FilesystemIdentityPolicy,
    exclusive: &mut BTreeMap<String, &'a RootRelativePath>,
    ranges: &RangesByIdentity<'_>,
) -> Result<(), IrError> {
    let key = identity_key(path, policy);
    if exclusive.insert(key.clone(), path).is_some() || ranges.contains_key(&key) {
        return Err(IrError::PathConflict(path.to_string()));
    }
    Ok(())
}

fn detect_move_cycles(
    moves: &BTreeMap<String, (String, &RootRelativePath, &RootRelativePath)>,
) -> Result<(), IrError> {
    for (start, (_, original, _)) in moves {
        let mut seen = BTreeSet::new();
        let mut current = start;
        while let Some((next, _, _)) = moves.get(current) {
            if !seen.insert(current) {
                return Err(IrError::MoveCycle(original.to_string()));
            }
            current = next;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IrError {
    InvalidPath(String),
    InvalidRevision(String),
    InvalidChangeDiffDigest,
    UnsupportedEncoding,
    BinaryContent,
    InvalidNewline,
    InvalidTextContent,
    InvalidRange {
        start: usize,
        end: usize,
    },
    UnsupportedVersion(u32),
    IdentityPolicyMismatch {
        expected: FilesystemIdentityPolicy,
        supplied: FilesystemIdentityPolicy,
    },
    OperationLimit {
        actual: usize,
        limit: usize,
    },
    ContentLimit {
        actual: usize,
        limit: usize,
    },
    InputLimit {
        actual: usize,
        limit: usize,
    },
    PathConflict(String),
    BaseDigestConflict(String),
    ExecutableModeConflict(String),
    OverlappingRanges(String),
    MoveCycle(String),
    MalformedIr(String),
    NonCanonical,
    Allocation,
}

impl fmt::Display for IrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => {
                write!(formatter, "invalid root-relative lexical path: {path}")
            }
            Self::InvalidRevision(revision) => {
                write!(formatter, "invalid workspace revision: {revision}")
            }
            Self::InvalidChangeDiffDigest => {
                formatter.write_str("invalid expected change-diff digest")
            }
            Self::UnsupportedEncoding => formatter.write_str("content is not UTF-8"),
            Self::BinaryContent => formatter.write_str("binary content is unsupported"),
            Self::InvalidNewline => {
                formatter.write_str("mixed, bare-CR, or ambiguous newlines are unsupported")
            }
            Self::InvalidTextContent => formatter
                .write_str("canonical text contains CR or has trailing LF without final_newline"),
            Self::InvalidRange { start, end } => {
                write!(formatter, "invalid byte range {start}..{end}")
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported edit IR version {version}")
            }
            Self::IdentityPolicyMismatch { expected, supplied } => write!(
                formatter,
                "edit IR identity policy {supplied} does not match active policy {expected}"
            ),
            Self::OperationLimit { actual, limit } => {
                write!(formatter, "operation count {actual} exceeds limit {limit}")
            }
            Self::ContentLimit { actual, limit } => {
                write!(formatter, "content bytes {actual} exceed limit {limit}")
            }
            Self::InputLimit { actual, limit } => {
                write!(formatter, "input bytes {actual} exceed limit {limit}")
            }
            Self::PathConflict(path) => write!(formatter, "conflicting operations for path {path}"),
            Self::BaseDigestConflict(path) => {
                write!(formatter, "conflicting base digests for path {path}")
            }
            Self::ExecutableModeConflict(path) => {
                write!(
                    formatter,
                    "conflicting executable mode outcomes for path {path}"
                )
            }
            Self::OverlappingRanges(path) => {
                write!(formatter, "overlapping or duplicate ranges for path {path}")
            }
            Self::MoveCycle(path) => write!(formatter, "move cycle containing path {path}"),
            Self::MalformedIr(reason) => write!(formatter, "malformed edit IR: {reason}"),
            Self::NonCanonical => formatter.write_str("edit IR bytes are not canonical"),
            Self::Allocation => formatter.write_str("edit IR allocation failed"),
        }
    }
}

impl std::error::Error for IrError {}
