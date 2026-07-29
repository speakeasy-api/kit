use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use url::Url;

use crate::{
    verify::lsp::session::{
        AcceptedNotification, AcceptedResponse, DocumentVersion, PendingToken, PositionEncoding,
        RequestId, ServerIdentity,
    },
    workspace::{
        edit::{
            ir::{
                ByteRange as IrByteRange, EditIr, EditLimits, EditOperation, ExecutableMode,
                Newline, RevisionToken, RootRelativePath, TextContent, identity_key,
            },
            normalize::BaseFile,
        },
        revision::RevisionId,
        syntax::SyntacticFact,
    },
};

const BTREE_ENTRY_OVERHEAD: usize = 16 * std::mem::size_of::<usize>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryFactClassification {
    Syntactic,
    Semantic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryFactProvenance {
    TreeSitter,
    Lsp,
}

pub trait ClassifiedRepositoryFact {
    fn classification(&self) -> RepositoryFactClassification;
    fn repository_provenance(&self) -> RepositoryFactProvenance;
}

impl<T> ClassifiedRepositoryFact for SyntacticFact<T> {
    fn classification(&self) -> RepositoryFactClassification {
        RepositoryFactClassification::Syntactic
    }

    fn repository_provenance(&self) -> RepositoryFactProvenance {
        RepositoryFactProvenance::TreeSitter
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticRelationKind {
    Declaration,
    Definition,
    TypeDefinition,
    Implementation,
    Reference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticByteRange {
    start: usize,
    end: usize,
}

impl SemanticByteRange {
    pub const fn start(&self) -> usize {
        self.start
    }

    pub const fn end(&self) -> usize {
        self.end
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticPath(RootRelativePath);

impl SemanticPath {
    pub fn as_path(&self) -> &RootRelativePath {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactOrigin {
    uri: Arc<str>,
    document_version: DocumentVersion,
    request_generation: u64,
    request_id: RequestId,
}

impl FactOrigin {
    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    pub const fn request_generation(&self) -> u64 {
        self.request_generation
    }

    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NormalizedConfidence {
    ExactSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticProvenance {
    classification: RepositoryFactClassification,
    source: RepositoryFactProvenance,
    revision: RevisionId,
    origin: FactOrigin,
    server: ServerIdentity,
    position_encoding: PositionEncoding,
    confidence: NormalizedConfidence,
}

impl SemanticProvenance {
    pub const fn classification(&self) -> RepositoryFactClassification {
        self.classification
    }

    pub const fn source(&self) -> RepositoryFactProvenance {
        self.source
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub const fn origin(&self) -> &FactOrigin {
        &self.origin
    }

    pub const fn server(&self) -> &ServerIdentity {
        &self.server
    }

    pub const fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub const fn confidence(&self) -> NormalizedConfidence {
        self.confidence
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticFact {
    relation: SemanticRelationKind,
    path: SemanticPath,
    range: SemanticByteRange,
    target_range: Option<SemanticByteRange>,
    origin_selection_range: Option<SemanticByteRange>,
    provenance: Arc<SemanticProvenance>,
}

impl SemanticFact {
    pub const fn relation(&self) -> SemanticRelationKind {
        self.relation
    }

    pub const fn path(&self) -> &SemanticPath {
        &self.path
    }

    pub const fn range(&self) -> &SemanticByteRange {
        &self.range
    }

    pub const fn target_range(&self) -> Option<&SemanticByteRange> {
        self.target_range.as_ref()
    }

    pub const fn origin_selection_range(&self) -> Option<&SemanticByteRange> {
        self.origin_selection_range.as_ref()
    }

    pub fn provenance(&self) -> &SemanticProvenance {
        &self.provenance
    }
}

impl ClassifiedRepositoryFact for SemanticFact {
    fn classification(&self) -> RepositoryFactClassification {
        RepositoryFactClassification::Semantic
    }

    fn repository_provenance(&self) -> RepositoryFactProvenance {
        RepositoryFactProvenance::Lsp
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FactLimits {
    pub max_facts: usize,
    pub max_diagnostics: usize,
    pub max_message_bytes: usize,
    pub max_code_bytes: usize,
    pub max_source_bytes: usize,
    pub max_uri_bytes: usize,
    pub max_document_bytes: usize,
    pub max_workspace_bytes: usize,
    pub max_open_documents: usize,
    pub max_open_document_bytes: usize,
    pub max_position_work_bytes: usize,
    pub max_retained_output_bytes: usize,
}

impl Default for FactLimits {
    fn default() -> Self {
        Self {
            max_facts: 10_000,
            max_diagnostics: 10_000,
            max_message_bytes: 64 * 1024,
            max_code_bytes: 1_024,
            max_source_bytes: 1_024,
            max_uri_bytes: 16 * 1024,
            max_document_bytes: 16 * 1024 * 1024,
            max_workspace_bytes: 256 * 1024 * 1024,
            max_open_documents: 4_096,
            max_open_document_bytes: 128 * 1024 * 1024,
            max_position_work_bytes: 32 * 1024 * 1024,
            max_retained_output_bytes: 32 * 1024 * 1024,
        }
    }
}

impl FactLimits {
    pub(crate) fn valid(self) -> bool {
        self.max_facts > 0
            && self.max_diagnostics > 0
            && self.max_message_bytes > 0
            && self.max_code_bytes > 0
            && self.max_source_bytes > 0
            && self.max_uri_bytes > 0
            && self.max_document_bytes > 0
            && self.max_workspace_bytes >= self.max_document_bytes
            && self.max_open_documents > 0
            && self.max_open_document_bytes > 0
            && self.max_position_work_bytes > 0
            && self.max_retained_output_bytes > 0
    }
}

#[derive(Clone, Debug)]
pub struct SnapshotFile {
    path: String,
    bytes: Vec<u8>,
    executable: bool,
}

impl SnapshotFile {
    pub fn new(path: impl Into<String>, bytes: Vec<u8>, executable: bool) -> Self {
        Self {
            path: path.into(),
            bytes,
            executable,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpenDocument {
    uri: String,
    version: DocumentVersion,
    text: String,
}

impl OpenDocument {
    pub fn new(uri: impl Into<String>, version: DocumentVersion, text: String) -> Self {
        Self {
            uri: uri.into(),
            version,
            text,
        }
    }
}

#[derive(Clone, Debug)]
struct SnapshotFileState {
    path: RootRelativePath,
    bytes: Vec<u8>,
    base: BaseFile,
    lines: Arc<LineIndex>,
}

#[derive(Clone, Debug)]
struct OpenDocumentState {
    uri: String,
    version: DocumentVersion,
    text: String,
    lines: Arc<LineIndex>,
}

#[derive(Clone, Debug)]
struct LineIndex(Vec<usize>);

impl LineIndex {
    fn new(text: &str) -> Result<Self, LspNormalizeError> {
        let mut starts = Vec::new();
        starts
            .try_reserve_exact(text.bytes().filter(|byte| *byte == b'\n').count() + 1)
            .map_err(|_| LspNormalizeError::LimitExceeded)?;
        starts.push(0);
        starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        Ok(Self(starts))
    }

    fn retained_bytes(&self) -> Result<usize, LspNormalizeError> {
        self.0
            .capacity()
            .checked_mul(std::mem::size_of::<usize>())
            .and_then(|value| value.checked_add(std::mem::size_of::<LineIndex>()))
            .and_then(|value| value.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(LspNormalizeError::LimitExceeded)
    }
}

fn line_index_retained_bytes(bytes: &[u8]) -> Result<usize, LspNormalizeError> {
    let mut lines = 1_usize;
    for byte in bytes {
        if *byte == b'\n' {
            lines = lines
                .checked_add(1)
                .ok_or(LspNormalizeError::LimitExceeded)?;
        }
    }
    lines
        .checked_mul(std::mem::size_of::<usize>())
        .and_then(|value| value.checked_add(std::mem::size_of::<LineIndex>()))
        .and_then(|value| value.checked_add(2 * std::mem::size_of::<usize>()))
        .ok_or(LspNormalizeError::LimitExceeded)
}

fn snapshot_file_retained_bytes(file: &SnapshotFile) -> Result<usize, LspNormalizeError> {
    file.bytes
        .capacity()
        .checked_add(file.bytes.len())
        .and_then(|value| value.checked_add(file.path.capacity()))
        .and_then(|value| value.checked_add(file.path.len().checked_mul(3)?))
        .and_then(|value| value.checked_add(71))
        .and_then(|value| value.checked_add(line_index_retained_bytes(&file.bytes).ok()?))
        .and_then(|value| value.checked_add(std::mem::size_of::<SnapshotFileState>()))
        .and_then(|value| value.checked_add(BTREE_ENTRY_OVERHEAD))
        .ok_or(LspNormalizeError::LimitExceeded)
}

fn open_document_retained_bytes(document: &OpenDocument) -> Result<usize, LspNormalizeError> {
    document
        .uri
        .capacity()
        .checked_mul(3)
        .and_then(|value| value.checked_add(document.text.capacity()))
        .and_then(|value| value.checked_add(document.uri.len().checked_mul(3)?))
        .and_then(|value| {
            value.checked_add(line_index_retained_bytes(document.text.as_bytes()).ok()?)
        })
        .and_then(|value| value.checked_add(std::mem::size_of::<OpenDocumentState>()))
        .and_then(|value| value.checked_add(3 * BTREE_ENTRY_OVERHEAD))
        .ok_or(LspNormalizeError::LimitExceeded)
}

#[derive(Clone, Copy)]
struct IndexedText<'a> {
    text: &'a str,
    lines: &'a LineIndex,
}

#[derive(Clone, Debug)]
pub struct LspWorkspaceSnapshot {
    root: PathBuf,
    revision: RevisionId,
    document_epoch: u64,
    files: BTreeMap<String, SnapshotFileState>,
    documents: BTreeMap<String, OpenDocumentState>,
    document_paths: BTreeMap<String, String>,
    server: ServerIdentity,
    position_encoding: PositionEncoding,
    edit_limits: EditLimits,
    fact_limits: FactLimits,
}

impl LspWorkspaceSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: PathBuf,
        revision: RevisionId,
        document_epoch: u64,
        files: Vec<SnapshotFile>,
        documents: Vec<OpenDocument>,
        server: ServerIdentity,
        position_encoding: PositionEncoding,
        edit_limits: EditLimits,
        fact_limits: FactLimits,
    ) -> Result<Self, LspNormalizeError> {
        if !root.is_absolute()
            || !root.is_dir()
            || root
                .canonicalize()
                .map_err(|_| LspNormalizeError::InvalidRoot)?
                != root
            || !fact_limits.valid()
            || edit_limits.max_operations == 0
            || edit_limits.max_path_bytes == 0
            || edit_limits.max_content_bytes == 0
            || edit_limits.max_input_bytes == 0
        {
            return Err(LspNormalizeError::InvalidSnapshot);
        }
        let mut workspace_bytes = root
            .to_string_lossy()
            .len()
            .checked_add(server_retained_bytes(&server))
            .and_then(|value| value.checked_add(std::mem::size_of::<Self>()))
            .and_then(|value| {
                value.checked_add(
                    files
                        .capacity()
                        .checked_mul(std::mem::size_of::<SnapshotFile>())?,
                )
            })
            .ok_or(LspNormalizeError::LimitExceeded)?;
        for file in &files {
            if file.bytes.len() > fact_limits.max_document_bytes {
                return Err(LspNormalizeError::LimitExceeded);
            }
            std::str::from_utf8(&file.bytes).map_err(|_| LspNormalizeError::UnsupportedEncoding)?;
            workspace_bytes = workspace_bytes
                .checked_add(snapshot_file_retained_bytes(file)?)
                .ok_or(LspNormalizeError::LimitExceeded)?;
        }
        if workspace_bytes > fact_limits.max_workspace_bytes {
            return Err(LspNormalizeError::LimitExceeded);
        }

        let mut states = BTreeMap::new();
        for mut file in files {
            file.path.shrink_to_fit();
            file.bytes.shrink_to_fit();
            let path = RootRelativePath::parse(file.path, edit_limits.max_path_bytes)?;
            let text = std::str::from_utf8(&file.bytes)
                .map_err(|_| LspNormalizeError::UnsupportedEncoding)?;
            let lines = Arc::new(LineIndex::new(text)?);
            let mut key = identity_key(&path, edit_limits.identity_policy);
            key.shrink_to_fit();
            let base = BaseFile::new(&file.bytes, file.executable)
                .map_err(|_| LspNormalizeError::InvalidSnapshot)?;
            if states
                .insert(
                    key.clone(),
                    SnapshotFileState {
                        path,
                        bytes: file.bytes,
                        base,
                        lines,
                    },
                )
                .is_some()
            {
                return Err(LspNormalizeError::DuplicatePath(key));
            }
        }
        let mut snapshot = Self {
            root,
            revision,
            document_epoch,
            files: states,
            documents: BTreeMap::new(),
            document_paths: BTreeMap::new(),
            server,
            position_encoding,
            edit_limits,
            fact_limits,
        };
        let mut document_paths = BTreeSet::new();
        let mut open_document_bytes = 0_usize;
        open_document_bytes = open_document_bytes
            .checked_add(
                documents
                    .capacity()
                    .checked_mul(std::mem::size_of::<OpenDocument>())
                    .ok_or(LspNormalizeError::LimitExceeded)?,
            )
            .ok_or(LspNormalizeError::LimitExceeded)?;
        for document in &documents {
            if document.uri.len() > fact_limits.max_uri_bytes
                || document.text.len() > fact_limits.max_document_bytes
            {
                return Err(LspNormalizeError::LimitExceeded);
            }
            open_document_bytes = open_document_bytes
                .checked_add(open_document_retained_bytes(document)?)
                .ok_or(LspNormalizeError::LimitExceeded)?;
        }
        if documents.len() > fact_limits.max_open_documents
            || open_document_bytes > fact_limits.max_open_document_bytes
        {
            return Err(LspNormalizeError::LimitExceeded);
        }

        open_document_bytes = 0;
        for mut document in documents {
            document.uri.shrink_to_fit();
            document.text.shrink_to_fit();
            if snapshot.documents.len() >= fact_limits.max_open_documents {
                return Err(LspNormalizeError::LimitExceeded);
            }
            if document.uri.len() > fact_limits.max_uri_bytes
                || document.text.len() > fact_limits.max_document_bytes
            {
                return Err(LspNormalizeError::LimitExceeded);
            }
            let path = snapshot.resolve_uri(&document.uri)?;
            let file = snapshot
                .file(&path)
                .ok_or_else(|| LspNormalizeError::UntrackedDocument(path.to_string()))?;
            let (lines, line_bytes) = if document.text.as_bytes() == file.bytes {
                (file.lines.clone(), 0)
            } else {
                let lines = Arc::new(LineIndex::new(&document.text)?);
                let bytes = lines.retained_bytes()?;
                (lines, bytes)
            };
            let document_uri = document.uri.clone();
            let mut path_key = identity_key(&path, edit_limits.identity_policy);
            path_key.shrink_to_fit();
            let retained = document
                .uri
                .capacity()
                .checked_mul(3)
                .and_then(|value| value.checked_add(document.text.capacity()))
                .and_then(|value| value.checked_add(path_key.capacity()))
                .and_then(|value| value.checked_add(line_bytes))
                .and_then(|value| value.checked_add(std::mem::size_of::<OpenDocumentState>()))
                .ok_or(LspNormalizeError::LimitExceeded)?;
            open_document_bytes = open_document_bytes
                .checked_add(retained)
                .ok_or(LspNormalizeError::LimitExceeded)?;
            if open_document_bytes > fact_limits.max_open_document_bytes {
                return Err(LspNormalizeError::LimitExceeded);
            }
            if !document_paths.insert(path_key.clone())
                || snapshot
                    .documents
                    .insert(
                        document.uri.clone(),
                        OpenDocumentState {
                            uri: document.uri,
                            version: document.version,
                            text: document.text,
                            lines,
                        },
                    )
                    .is_some()
            {
                return Err(LspNormalizeError::DuplicatePath(path.to_string()));
            }
            snapshot.document_paths.insert(path_key, document_uri);
        }
        Ok(snapshot)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub const fn document_epoch(&self) -> u64 {
        self.document_epoch
    }

    pub const fn server(&self) -> &ServerIdentity {
        &self.server
    }

    pub const fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub const fn edit_limits(&self) -> EditLimits {
        self.edit_limits
    }

    pub const fn fact_limits(&self) -> FactLimits {
        self.fact_limits
    }

    pub fn resolve_uri(&self, uri: &str) -> Result<RootRelativePath, LspNormalizeError> {
        if uri.is_empty() || uri.len() > self.fact_limits.max_uri_bytes {
            return Err(LspNormalizeError::InvalidUri);
        }
        let parsed = Url::parse(uri).map_err(|_| LspNormalizeError::InvalidUri)?;
        if parsed.scheme() != "file"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed
                .host_str()
                .is_some_and(|host| !host.eq_ignore_ascii_case("localhost"))
        {
            return Err(LspNormalizeError::InvalidUri);
        }
        let raw_path = raw_file_uri_path(uri).ok_or(LspNormalizeError::InvalidUri)?;
        if raw_path.contains('\\') || encoded_separator_or_nul(raw_path) {
            return Err(LspNormalizeError::InvalidUri);
        }
        let decoded = percent_decode_utf8(raw_path)?;
        let absolute = Path::new(&decoded);
        if !absolute.is_absolute() {
            return Err(LspNormalizeError::InvalidUri);
        }
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| LspNormalizeError::OutsideWorkspace)?;
        let mut value = String::new();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(LspNormalizeError::InvalidUri);
            };
            let component = component.to_str().ok_or(LspNormalizeError::InvalidUri)?;
            if component.is_empty() || matches!(component, "." | "..") {
                return Err(LspNormalizeError::InvalidUri);
            }
            if !value.is_empty() {
                value.push('/');
            }
            value.push_str(component);
        }
        value.shrink_to_fit();
        RootRelativePath::parse(value, self.edit_limits.max_path_bytes).map_err(Into::into)
    }

    fn require_revision(&self, revision: RevisionId) -> Result<(), LspNormalizeError> {
        if revision == self.revision {
            Ok(())
        } else {
            Err(LspNormalizeError::StaleWorkspaceRevision)
        }
    }

    fn file(&self, path: &RootRelativePath) -> Option<&SnapshotFileState> {
        self.files
            .get(&identity_key(path, self.edit_limits.identity_policy))
    }

    fn document(&self, uri: &str) -> Option<&OpenDocumentState> {
        self.documents.get(uri)
    }

    fn document_for_path(&self, path: &RootRelativePath) -> Option<&OpenDocumentState> {
        let key = identity_key(path, self.edit_limits.identity_policy);
        self.document_paths
            .get(&key)
            .and_then(|uri| self.documents.get(uri))
    }

    fn text_for_uri<'a>(
        &'a self,
        uri: &str,
        path: &RootRelativePath,
    ) -> Result<IndexedText<'a>, LspNormalizeError> {
        if let Some(document) = self.document(uri).or_else(|| self.document_for_path(path)) {
            return Ok(IndexedText {
                text: &document.text,
                lines: &document.lines,
            });
        }
        let file = self
            .file(path)
            .ok_or_else(|| LspNormalizeError::UnknownPath(path.to_string()))?;
        Ok(IndexedText {
            text: std::str::from_utf8(&file.bytes)
                .map_err(|_| LspNormalizeError::UnsupportedEncoding)?,
            lines: &file.lines,
        })
    }
}

pub fn normalize_semantic_locations(
    snapshot: &LspWorkspaceSnapshot,
    accepted: &AcceptedResponse,
) -> Result<Vec<SemanticFact>, LspNormalizeError> {
    let relation = match accepted.token().method.as_str() {
        "textDocument/declaration" => SemanticRelationKind::Declaration,
        "textDocument/definition" => SemanticRelationKind::Definition,
        "textDocument/typeDefinition" => SemanticRelationKind::TypeDefinition,
        "textDocument/implementation" => SemanticRelationKind::Implementation,
        "textDocument/references" => SemanticRelationKind::Reference,
        _ => return Err(LspNormalizeError::UnsupportedMethod),
    };
    validate_accepted(snapshot, accepted)?;
    let result = accepted
        .result()
        .ok_or(LspNormalizeError::ServerErrorResponse)?;
    let result_is_array = result.is_array();
    let values = match result {
        Value::Null => return Ok(Vec::new()),
        Value::Array(values) => values.as_slice(),
        Value::Object(_) => std::slice::from_ref(result),
        _ => return Err(LspNormalizeError::MalformedPayload),
    };
    if values.len() > snapshot.fact_limits.max_facts {
        return Err(LspNormalizeError::LimitExceeded);
    }
    let mut output_budget = OutputBudget::new(snapshot.fact_limits.max_retained_output_bytes);
    output_budget.charge(
        accepted
            .token()
            .uri
            .len()
            .checked_add(server_retained_bytes(&accepted.token().server))
            .and_then(|value| value.checked_add(std::mem::size_of::<SemanticProvenance>()))
            .and_then(|value| value.checked_add(4 * std::mem::size_of::<usize>()))
            .ok_or(LspNormalizeError::LimitExceeded)?,
    )?;
    let origin = origin(accepted.token());
    let provenance = Arc::new(SemanticProvenance {
        classification: RepositoryFactClassification::Semantic,
        source: RepositoryFactProvenance::Lsp,
        revision: snapshot.revision,
        origin,
        server: accepted.token().server.clone(),
        position_encoding: accepted.token().position_encoding,
        confidence: NormalizedConfidence::ExactSource,
    });
    let mut position_budget = PositionBudget::new(snapshot.fact_limits.max_position_work_bytes);
    let mut facts = Vec::with_capacity(values.len());
    let mut representation = None;
    for value in values {
        let is_link = value.get("targetUri").is_some();
        if is_link && !result_is_array {
            return Err(LspNormalizeError::MalformedPayload);
        }
        if representation
            .replace(is_link)
            .is_some_and(|current| current != is_link)
        {
            return Err(LspNormalizeError::MalformedPayload);
        }
        let (uri, range, target_range, origin_selection_range) = if is_link {
            let link: LocationLinkWire = serde_json::from_value(value.clone())
                .map_err(|_| LspNormalizeError::MalformedPayload)?;
            let path = snapshot.resolve_uri(&link.target_uri)?;
            let text = snapshot.text_for_uri(&link.target_uri, &path)?;
            let target = convert_range(
                text,
                link.target_range,
                snapshot.position_encoding,
                &mut position_budget,
            )?;
            let selection = convert_range(
                text,
                link.target_selection_range,
                snapshot.position_encoding,
                &mut position_budget,
            )?;
            if selection.start < target.start || selection.end > target.end {
                return Err(LspNormalizeError::MalformedRange);
            }
            let origin_range = link
                .origin_selection_range
                .map(|range| {
                    let token = accepted.token();
                    let origin_path = snapshot.resolve_uri(&token.uri)?;
                    let origin_text = snapshot.text_for_uri(&token.uri, &origin_path)?;
                    convert_range(
                        origin_text,
                        range,
                        snapshot.position_encoding,
                        &mut position_budget,
                    )
                })
                .transpose()?;
            (link.target_uri, selection, Some(target), origin_range)
        } else {
            let location: LocationWire = serde_json::from_value(value.clone())
                .map_err(|_| LspNormalizeError::MalformedPayload)?;
            let path = snapshot.resolve_uri(&location.uri)?;
            let text = snapshot.text_for_uri(&location.uri, &path)?;
            let range = convert_range(
                text,
                location.range,
                snapshot.position_encoding,
                &mut position_budget,
            )?;
            (location.uri, range, None, None)
        };
        let path = snapshot.resolve_uri(&uri)?;
        output_budget.charge(
            path.as_str()
                .len()
                .checked_add(std::mem::size_of::<SemanticFact>())
                .ok_or(LspNormalizeError::LimitExceeded)?,
        )?;
        facts.push(SemanticFact {
            relation,
            path: SemanticPath(path),
            range,
            target_range,
            origin_selection_range,
            provenance: provenance.clone(),
        });
    }
    Ok(facts)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticCode {
    Integer(i64),
    String(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveDiagnostic {
    path: SemanticPath,
    range: SemanticByteRange,
    severity: Option<u8>,
    code: Option<DiagnosticCode>,
    source: Option<String>,
    message: String,
    provenance: Arc<DiagnosticProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticProvenance {
    classification: RepositoryFactClassification,
    source: RepositoryFactProvenance,
    revision: RevisionId,
    uri: String,
    document_version: DocumentVersion,
    server: ServerIdentity,
    position_encoding: PositionEncoding,
    confidence: NormalizedConfidence,
}

impl DiagnosticProvenance {
    pub const fn classification(&self) -> RepositoryFactClassification {
        self.classification
    }

    pub const fn source(&self) -> RepositoryFactProvenance {
        self.source
    }

    pub const fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    pub const fn server(&self) -> &ServerIdentity {
        &self.server
    }

    pub const fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub const fn confidence(&self) -> NormalizedConfidence {
        self.confidence
    }
}

impl LiveDiagnostic {
    pub const fn path(&self) -> &SemanticPath {
        &self.path
    }

    pub const fn range(&self) -> &SemanticByteRange {
        &self.range
    }

    pub const fn severity(&self) -> Option<u8> {
        self.severity
    }

    pub const fn code(&self) -> Option<&DiagnosticCode> {
        self.code.as_ref()
    }

    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn provenance(&self) -> &DiagnosticProvenance {
        &self.provenance
    }
}

impl ClassifiedRepositoryFact for LiveDiagnostic {
    fn classification(&self) -> RepositoryFactClassification {
        RepositoryFactClassification::Semantic
    }

    fn repository_provenance(&self) -> RepositoryFactProvenance {
        RepositoryFactProvenance::Lsp
    }
}

pub fn normalize_live_diagnostics(
    snapshot: &LspWorkspaceSnapshot,
    accepted: &AcceptedNotification,
) -> Result<Vec<LiveDiagnostic>, LspNormalizeError> {
    validate_notification(snapshot, accepted)?;
    let wire: PublishDiagnosticsWire = serde_json::from_value(accepted.payload().clone())
        .map_err(|_| LspNormalizeError::MalformedPayload)?;
    if wire.uri.len() > snapshot.fact_limits.max_uri_bytes
        || wire.diagnostics.len() > snapshot.fact_limits.max_diagnostics
    {
        return Err(LspNormalizeError::LimitExceeded);
    }
    if wire.uri != accepted.uri() || wire.version != accepted.document_version().get() {
        return Err(LspNormalizeError::StaleDocumentVersion);
    }
    let path = snapshot.resolve_uri(&wire.uri)?;
    let document = snapshot
        .document(&wire.uri)
        .ok_or(LspNormalizeError::DocumentNotOpen)?;
    if wire.version != document.version.get() {
        return Err(if wire.version < document.version.get() {
            LspNormalizeError::StaleDocumentVersion
        } else {
            LspNormalizeError::FutureDocumentVersion
        });
    }
    let mut output_budget = OutputBudget::new(snapshot.fact_limits.max_retained_output_bytes);
    output_budget.charge(
        document
            .uri
            .len()
            .checked_add(server_retained_bytes(accepted.server()))
            .and_then(|value| value.checked_add(std::mem::size_of::<DiagnosticProvenance>()))
            .and_then(|value| value.checked_add(4 * std::mem::size_of::<usize>()))
            .ok_or(LspNormalizeError::LimitExceeded)?,
    )?;
    let provenance = Arc::new(DiagnosticProvenance {
        classification: RepositoryFactClassification::Semantic,
        source: RepositoryFactProvenance::Lsp,
        revision: snapshot.revision,
        uri: document.uri.clone(),
        document_version: document.version,
        server: accepted.server().clone(),
        position_encoding: snapshot.position_encoding,
        confidence: NormalizedConfidence::ExactSource,
    });
    let mut position_budget = PositionBudget::new(snapshot.fact_limits.max_position_work_bytes);
    let mut diagnostics = Vec::with_capacity(wire.diagnostics.len());
    for mut diagnostic in wire.diagnostics {
        diagnostic.message.shrink_to_fit();
        if let Some(source) = &mut diagnostic.source {
            source.shrink_to_fit();
        }
        if diagnostic.message.len() > snapshot.fact_limits.max_message_bytes
            || diagnostic
                .source
                .as_ref()
                .is_some_and(|source| source.len() > snapshot.fact_limits.max_source_bytes)
        {
            return Err(LspNormalizeError::LimitExceeded);
        }
        let severity = diagnostic
            .severity
            .map(|severity| {
                u8::try_from(severity)
                    .ok()
                    .filter(|value| (1..=4).contains(value))
                    .ok_or(LspNormalizeError::MalformedPayload)
            })
            .transpose()?;
        let code = diagnostic
            .code
            .map(|code| match code {
                DiagnosticCodeWire::Integer(value) => Ok(DiagnosticCode::Integer(value)),
                DiagnosticCodeWire::String(mut value)
                    if value.len() <= snapshot.fact_limits.max_code_bytes =>
                {
                    value.shrink_to_fit();
                    Ok(DiagnosticCode::String(value))
                }
                DiagnosticCodeWire::String(_) => Err(LspNormalizeError::LimitExceeded),
            })
            .transpose()?;
        let retained = path
            .as_str()
            .len()
            .checked_add(diagnostic.message.len())
            .and_then(|value| value.checked_add(diagnostic.source.as_ref().map_or(0, String::len)))
            .and_then(|value| {
                value.checked_add(match &code {
                    Some(DiagnosticCode::String(value)) => value.len(),
                    Some(DiagnosticCode::Integer(_)) => std::mem::size_of::<i64>(),
                    None => 0,
                })
            })
            .and_then(|value| value.checked_add(std::mem::size_of::<LiveDiagnostic>()))
            .ok_or(LspNormalizeError::LimitExceeded)?;
        output_budget.charge(retained)?;
        diagnostics.push(LiveDiagnostic {
            path: SemanticPath(path.clone()),
            range: convert_range(
                IndexedText {
                    text: &document.text,
                    lines: &document.lines,
                },
                diagnostic.range,
                snapshot.position_encoding,
                &mut position_budget,
            )?,
            severity,
            code,
            source: diagnostic.source,
            message: diagnostic.message,
            provenance: provenance.clone(),
        });
    }
    Ok(diagnostics)
}

pub(crate) fn extend_bounded_diagnostics(
    output: &mut Vec<LiveDiagnostic>,
    retained_bytes: &mut usize,
    diagnostics: Vec<LiveDiagnostic>,
    max_diagnostics: usize,
    max_retained_bytes: usize,
) -> Result<(), LspNormalizeError> {
    let count = output
        .len()
        .checked_add(diagnostics.len())
        .filter(|count| *count <= max_diagnostics)
        .ok_or(LspNormalizeError::LimitExceeded)?;
    let additional = diagnostics.iter().try_fold(0_usize, |total, diagnostic| {
        total
            .checked_add(diagnostic.path.0.as_str().len())
            .and_then(|value| value.checked_add(diagnostic.message.len()))
            .and_then(|value| value.checked_add(diagnostic.source.as_ref().map_or(0, String::len)))
            .and_then(|value| {
                value.checked_add(match &diagnostic.code {
                    Some(DiagnosticCode::String(code)) => code.len(),
                    Some(DiagnosticCode::Integer(_)) => std::mem::size_of::<i64>(),
                    None => 0,
                })
            })
            .and_then(|value| value.checked_add(diagnostic.provenance.uri.len()))
            .and_then(|value| {
                value.checked_add(server_retained_bytes(&diagnostic.provenance.server))
            })
            .and_then(|value| value.checked_add(std::mem::size_of::<LiveDiagnostic>()))
            .and_then(|value| value.checked_add(std::mem::size_of::<DiagnosticProvenance>()))
            .ok_or(LspNormalizeError::LimitExceeded)
    })?;
    let total = (*retained_bytes)
        .checked_add(additional)
        .filter(|total| *total <= max_retained_bytes)
        .ok_or(LspNormalizeError::LimitExceeded)?;
    output
        .try_reserve_exact(count - output.len())
        .map_err(|_| LspNormalizeError::LimitExceeded)?;
    output.extend(diagnostics);
    *retained_bytes = total;
    Ok(())
}

pub fn normalize_workspace_edit(
    snapshot: &LspWorkspaceSnapshot,
    accepted: &AcceptedResponse,
) -> Result<EditIr, LspNormalizeError> {
    validate_accepted(snapshot, accepted)?;
    if !matches!(
        accepted.token().method.as_str(),
        "textDocument/rename"
            | "workspace/willCreateFiles"
            | "workspace/willRenameFiles"
            | "workspace/willDeleteFiles"
    ) {
        return Err(LspNormalizeError::UnsupportedMethod);
    }
    let result = accepted
        .result()
        .ok_or(LspNormalizeError::ServerErrorResponse)?;
    let edit: WorkspaceEditWire =
        serde_json::from_value(result.clone()).map_err(|_| LspNormalizeError::MalformedPayload)?;
    if edit.changes.is_some() && edit.document_changes.is_some() {
        return Err(LspNormalizeError::AmbiguousOrdering);
    }
    for (id, annotation) in &edit.change_annotations {
        if id.len() > snapshot.fact_limits.max_code_bytes
            || annotation.needs_confirmation == Some(true)
            || annotation.label.len() > snapshot.fact_limits.max_message_bytes
            || annotation
                .description
                .as_ref()
                .is_some_and(|value| value.len() > snapshot.fact_limits.max_message_bytes)
        {
            return Err(LspNormalizeError::UnsupportedSemantics);
        }
    }

    let mut sequence = Vec::new();
    let mut raw_operations = edit.change_annotations.len();
    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            raw_operations = raw_operations
                .checked_add(1)
                .and_then(|value| value.checked_add(edits.len()))
                .ok_or(LspNormalizeError::LimitExceeded)?;
            sequence.push(ParsedChange::Text(uri, VersionField::Null, edits));
        }
    }
    if let Some(document_changes) = edit.document_changes {
        for change in document_changes {
            match change {
                DocumentChangeWire::Text(edit) => {
                    raw_operations = raw_operations
                        .checked_add(1)
                        .and_then(|value| value.checked_add(edit.edits.len()))
                        .ok_or(LspNormalizeError::LimitExceeded)?;
                    sequence.push(ParsedChange::Text(
                        edit.text_document.uri,
                        edit.text_document.version,
                        edit.edits,
                    ));
                }
                DocumentChangeWire::Create(edit) => {
                    raw_operations = raw_operations
                        .checked_add(1)
                        .ok_or(LspNormalizeError::LimitExceeded)?;
                    sequence.push(ParsedChange::Create(edit));
                }
                DocumentChangeWire::Rename(edit) => {
                    raw_operations = raw_operations
                        .checked_add(1)
                        .ok_or(LspNormalizeError::LimitExceeded)?;
                    sequence.push(ParsedChange::Rename(edit));
                }
                DocumentChangeWire::Delete(edit) => {
                    raw_operations = raw_operations
                        .checked_add(1)
                        .ok_or(LspNormalizeError::LimitExceeded)?;
                    sequence.push(ParsedChange::Delete(edit));
                }
            }
        }
    }
    if raw_operations > snapshot.edit_limits.max_operations {
        return Err(LspNormalizeError::LimitExceeded);
    }

    let mut claimed = BTreeSet::new();
    let mut operations = Vec::new();
    let mut position_budget = PositionBudget::new(snapshot.fact_limits.max_position_work_bytes);
    let mut content_bytes = 0_usize;
    for change in sequence {
        match change {
            ParsedChange::Text(uri, version, edits) => {
                if edits.is_empty() {
                    return Err(LspNormalizeError::UnsupportedSemantics);
                }
                let path = snapshot.resolve_uri(&uri)?;
                claim_path(snapshot, &mut claimed, &path)?;
                let file = snapshot
                    .file(&path)
                    .ok_or_else(|| LspNormalizeError::UnknownPath(path.to_string()))?;
                require_clean_document(snapshot, &uri, file)?;
                match version {
                    VersionField::Missing => {
                        return Err(LspNormalizeError::MissingDocumentVersion);
                    }
                    VersionField::Null => {}
                    VersionField::Number(version) => {
                        let document = snapshot
                            .document(&uri)
                            .ok_or(LspNormalizeError::DocumentNotOpen)?;
                        if document.version.get() != version {
                            return Err(LspNormalizeError::StaleDocumentVersion);
                        }
                    }
                }
                let replacement = apply_text_edits(
                    snapshot,
                    file,
                    edits,
                    &edit.change_annotations,
                    &mut position_budget,
                    &mut content_bytes,
                )?;
                if replacement != file.bytes {
                    operations.push(EditOperation::ReplaceRange {
                        path: file.path.clone(),
                        base_digest: file.base.digest().clone(),
                        range: IrByteRange::new(0, file.bytes.len())?,
                        expected: file.base.content().clone(),
                        replacement: TextContent::from_bytes(&replacement)?,
                        executable: ExecutableMode::Preserve,
                    });
                }
            }
            ParsedChange::Create(create) => {
                require_annotation(create.annotation_id.as_deref(), &edit.change_annotations)?;
                if create.options.is_some_and(|options| {
                    options.overwrite == Some(true) || options.ignore_if_exists == Some(true)
                }) {
                    return Err(LspNormalizeError::UnsupportedSemantics);
                }
                let path = snapshot.resolve_uri(&create.uri)?;
                claim_path(snapshot, &mut claimed, &path)?;
                if snapshot.file(&path).is_some() || snapshot.document_for_path(&path).is_some() {
                    return Err(LspNormalizeError::PathAlreadyExists(path.to_string()));
                }
                operations.push(EditOperation::AddFile {
                    path,
                    content: TextContent::empty(Newline::Lf),
                    executable: false,
                });
            }
            ParsedChange::Rename(rename) => {
                require_annotation(rename.annotation_id.as_deref(), &edit.change_annotations)?;
                if rename.options.is_some_and(|options| {
                    options.overwrite == Some(true) || options.ignore_if_exists == Some(true)
                }) {
                    return Err(LspNormalizeError::UnsupportedSemantics);
                }
                let from = snapshot.resolve_uri(&rename.old_uri)?;
                let to = snapshot.resolve_uri(&rename.new_uri)?;
                claim_path(snapshot, &mut claimed, &from)?;
                claim_path(snapshot, &mut claimed, &to)?;
                let source = snapshot
                    .file(&from)
                    .ok_or_else(|| LspNormalizeError::UnknownPath(from.to_string()))?;
                require_clean_path(snapshot, source)?;
                if snapshot.file(&to).is_some() || snapshot.document_for_path(&to).is_some() {
                    return Err(LspNormalizeError::PathAlreadyExists(to.to_string()));
                }
                operations.push(EditOperation::MoveFile {
                    from,
                    to,
                    base_digest: source.base.digest().clone(),
                });
            }
            ParsedChange::Delete(delete) => {
                require_annotation(delete.annotation_id.as_deref(), &edit.change_annotations)?;
                if delete.options.is_some_and(|options| {
                    options.recursive == Some(true) || options.ignore_if_not_exists == Some(true)
                }) {
                    return Err(LspNormalizeError::UnsupportedSemantics);
                }
                let path = snapshot.resolve_uri(&delete.uri)?;
                claim_path(snapshot, &mut claimed, &path)?;
                let file = snapshot
                    .file(&path)
                    .ok_or_else(|| LspNormalizeError::UnknownPath(path.to_string()))?;
                require_clean_path(snapshot, file)?;
                operations.push(EditOperation::DeleteFile {
                    path,
                    base_digest: file.base.digest().clone(),
                });
            }
        }
    }
    if operations.is_empty() {
        return Err(LspNormalizeError::NoEffectiveChanges);
    }
    EditIr::new(
        RevisionToken::parse(accepted.token().workspace_revision.to_string())?,
        operations,
        snapshot.edit_limits,
    )
    .map_err(Into::into)
}

fn apply_text_edits(
    snapshot: &LspWorkspaceSnapshot,
    file: &SnapshotFileState,
    edits: Vec<TextEditWire>,
    annotations: &BTreeMap<String, ChangeAnnotationWire>,
    position_budget: &mut PositionBudget,
    content_bytes: &mut usize,
) -> Result<Vec<u8>, LspNormalizeError> {
    if edits.len() > snapshot.edit_limits.max_operations {
        return Err(LspNormalizeError::LimitExceeded);
    }
    let source =
        std::str::from_utf8(&file.bytes).map_err(|_| LspNormalizeError::UnsupportedEncoding)?;
    let indexed = IndexedText {
        text: source,
        lines: &file.lines,
    };
    let mut converted = Vec::with_capacity(edits.len());
    let mut replacement_bytes = 0_usize;
    for edit in edits {
        require_annotation(edit.annotation_id.as_deref(), annotations)?;
        replacement_bytes = replacement_bytes
            .checked_add(edit.new_text.len())
            .ok_or(LspNormalizeError::LimitExceeded)?;
        if replacement_bytes > snapshot.edit_limits.max_content_bytes {
            return Err(LspNormalizeError::LimitExceeded);
        }
        let range = convert_range(
            indexed,
            edit.range,
            snapshot.position_encoding,
            position_budget,
        )?;
        converted.push((range.start, range.end, edit.new_text));
    }
    converted.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in converted.windows(2) {
        if pair[1].0 < pair[0].1 || pair[1].0 == pair[0].0 {
            return Err(LspNormalizeError::OverlappingEdits);
        }
    }
    let removed = converted
        .iter()
        .try_fold(0_usize, |total, (start, end, _)| {
            total
                .checked_add(end - start)
                .ok_or(LspNormalizeError::LimitExceeded)
        })?;
    let final_len = file
        .bytes
        .len()
        .checked_sub(removed)
        .and_then(|value| value.checked_add(replacement_bytes))
        .ok_or(LspNormalizeError::LimitExceeded)?;
    if final_len > snapshot.edit_limits.max_content_bytes {
        return Err(LspNormalizeError::LimitExceeded);
    }
    *content_bytes = content_bytes
        .checked_add(file.bytes.len())
        .and_then(|value| value.checked_add(final_len))
        .ok_or(LspNormalizeError::LimitExceeded)?;
    if *content_bytes > snapshot.edit_limits.max_content_bytes {
        return Err(LspNormalizeError::LimitExceeded);
    }
    let mut result = Vec::new();
    result
        .try_reserve_exact(final_len)
        .map_err(|_| LspNormalizeError::LimitExceeded)?;
    let mut cursor = 0_usize;
    for (start, end, replacement) in converted {
        result.extend_from_slice(&file.bytes[cursor..start]);
        result.extend_from_slice(replacement.as_bytes());
        cursor = end;
    }
    result.extend_from_slice(&file.bytes[cursor..]);
    Ok(result)
}

fn require_clean_path(
    snapshot: &LspWorkspaceSnapshot,
    file: &SnapshotFileState,
) -> Result<(), LspNormalizeError> {
    if snapshot
        .document_for_path(&file.path)
        .is_some_and(|document| document.text.as_bytes() != file.bytes)
    {
        Err(LspNormalizeError::UnsavedDocument)
    } else {
        Ok(())
    }
}

fn require_clean_document(
    snapshot: &LspWorkspaceSnapshot,
    uri: &str,
    file: &SnapshotFileState,
) -> Result<(), LspNormalizeError> {
    if snapshot
        .document(uri)
        .or_else(|| snapshot.document_for_path(&file.path))
        .is_some_and(|document| document.text.as_bytes() != file.bytes)
    {
        Err(LspNormalizeError::UnsavedDocument)
    } else {
        Ok(())
    }
}

fn claim_path(
    snapshot: &LspWorkspaceSnapshot,
    claimed: &mut BTreeSet<String>,
    path: &RootRelativePath,
) -> Result<(), LspNormalizeError> {
    if claimed.insert(identity_key(path, snapshot.edit_limits.identity_policy)) {
        Ok(())
    } else {
        Err(LspNormalizeError::AmbiguousOrdering)
    }
}

fn require_annotation(
    id: Option<&str>,
    annotations: &BTreeMap<String, ChangeAnnotationWire>,
) -> Result<(), LspNormalizeError> {
    if id.is_some_and(|id| !annotations.contains_key(id)) {
        Err(LspNormalizeError::UnknownAnnotation)
    } else {
        Ok(())
    }
}

fn validate_accepted(
    snapshot: &LspWorkspaceSnapshot,
    accepted: &AcceptedResponse,
) -> Result<(), LspNormalizeError> {
    let token = accepted.token();
    snapshot.require_revision(token.workspace_revision)?;
    if token.document_epoch != snapshot.document_epoch {
        return Err(LspNormalizeError::StaleDocumentEpoch);
    }
    if token.server != snapshot.server {
        return Err(LspNormalizeError::ServerMismatch);
    }
    if token.position_encoding != snapshot.position_encoding {
        return Err(LspNormalizeError::PositionEncodingMismatch);
    }
    let document = snapshot
        .document(&token.uri)
        .ok_or(LspNormalizeError::DocumentNotOpen)?;
    if document.version != token.document_version {
        return Err(LspNormalizeError::StaleDocumentVersion);
    }
    Ok(())
}

fn validate_notification(
    snapshot: &LspWorkspaceSnapshot,
    accepted: &AcceptedNotification,
) -> Result<(), LspNormalizeError> {
    snapshot.require_revision(accepted.workspace_revision())?;
    if accepted.document_epoch() != snapshot.document_epoch {
        return Err(LspNormalizeError::StaleDocumentEpoch);
    }
    if accepted.server() != &snapshot.server {
        return Err(LspNormalizeError::ServerMismatch);
    }
    if accepted.position_encoding() != snapshot.position_encoding {
        return Err(LspNormalizeError::PositionEncodingMismatch);
    }
    let document = snapshot
        .document(accepted.uri())
        .ok_or(LspNormalizeError::DocumentNotOpen)?;
    if document.version != accepted.document_version() {
        return Err(if accepted.document_version() < document.version {
            LspNormalizeError::StaleDocumentVersion
        } else {
            LspNormalizeError::FutureDocumentVersion
        });
    }
    Ok(())
}

fn origin(token: &PendingToken) -> FactOrigin {
    FactOrigin {
        uri: Arc::from(token.uri.as_str()),
        document_version: token.document_version,
        request_generation: token.generation,
        request_id: token.request_id,
    }
}

fn convert_range(
    text: IndexedText<'_>,
    range: RangeWire,
    encoding: PositionEncoding,
    budget: &mut PositionBudget,
) -> Result<SemanticByteRange, LspNormalizeError> {
    let start = convert_position(text, range.start, encoding, budget)?;
    let end = convert_position(text, range.end, encoding, budget)?;
    if start > end {
        return Err(LspNormalizeError::MalformedRange);
    }
    Ok(SemanticByteRange { start, end })
}

fn convert_position(
    indexed: IndexedText<'_>,
    position: PositionWire,
    encoding: PositionEncoding,
    budget: &mut PositionBudget,
) -> Result<usize, LspNormalizeError> {
    let target_line =
        usize::try_from(position.line).map_err(|_| LspNormalizeError::MalformedRange)?;
    let target_units =
        usize::try_from(position.character).map_err(|_| LspNormalizeError::MalformedRange)?;
    let start = *indexed
        .lines
        .0
        .get(target_line)
        .ok_or(LspNormalizeError::MalformedRange)?;
    let physical_end = indexed
        .lines
        .0
        .get(target_line + 1)
        .map_or(indexed.text.len(), |next| next - 1);
    let end = if physical_end > start && indexed.text.as_bytes()[physical_end - 1] == b'\r' {
        physical_end - 1
    } else {
        physical_end
    };
    let line = &indexed.text[start..end];
    let offset = match encoding {
        PositionEncoding::Utf8 => {
            budget.charge(target_units.min(line.len()).saturating_add(1))?;
            if target_units > line.len() || !line.is_char_boundary(target_units) {
                return Err(LspNormalizeError::MalformedRange);
            }
            target_units
        }
        PositionEncoding::Utf16 => units_offset(line, target_units, char::len_utf16, budget)?,
        PositionEncoding::Utf32 => units_offset(line, target_units, |_| 1, budget)?,
    };
    Ok(start + offset)
}

fn units_offset(
    line: &str,
    target: usize,
    units: impl Fn(char) -> usize,
    budget: &mut PositionBudget,
) -> Result<usize, LspNormalizeError> {
    let mut consumed = 0;
    for (offset, character) in line.char_indices() {
        budget.charge(character.len_utf8())?;
        if consumed == target {
            return Ok(offset);
        }
        consumed = consumed
            .checked_add(units(character))
            .ok_or(LspNormalizeError::MalformedRange)?;
        if consumed > target {
            return Err(LspNormalizeError::MalformedRange);
        }
    }
    if consumed == target {
        Ok(line.len())
    } else {
        Err(LspNormalizeError::MalformedRange)
    }
}

struct PositionBudget {
    remaining: usize,
}

impl PositionBudget {
    const fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    fn charge(&mut self, amount: usize) -> Result<(), LspNormalizeError> {
        self.remaining = self
            .remaining
            .checked_sub(amount)
            .ok_or(LspNormalizeError::LimitExceeded)?;
        Ok(())
    }
}

struct OutputBudget {
    remaining: usize,
}

impl OutputBudget {
    const fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    fn charge(&mut self, amount: usize) -> Result<(), LspNormalizeError> {
        self.remaining = self
            .remaining
            .checked_sub(amount)
            .ok_or(LspNormalizeError::LimitExceeded)?;
        Ok(())
    }
}

fn server_retained_bytes(server: &ServerIdentity) -> usize {
    server.server_artifact.as_str().len() + server.configuration.as_str().len()
}

fn raw_file_uri_path(uri: &str) -> Option<&str> {
    let colon = uri.find(':')?;
    let rest = &uri[colon + 1..];
    if let Some(authority) = rest.strip_prefix("//") {
        let slash = authority.find('/')?;
        Some(&authority[slash..])
    } else {
        Some(rest)
    }
}

fn encoded_separator_or_nul(value: &str) -> bool {
    value.as_bytes().windows(3).any(|window| {
        window[0] == b'%'
            && matches!(
                (
                    window[1].to_ascii_lowercase(),
                    window[2].to_ascii_lowercase()
                ),
                (b'2', b'f') | (b'5', b'c') | (b'0', b'0')
            )
    })
}

fn percent_decode_utf8(value: &str) -> Result<String, LspNormalizeError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1).ok_or(LspNormalizeError::InvalidUri)?;
            let low = *bytes.get(index + 2).ok_or(LspNormalizeError::InvalidUri)?;
            decoded.push((hex(high)? << 4) | hex(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| LspNormalizeError::InvalidUri)
}

fn hex(value: u8) -> Result<u8, LspNormalizeError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(LspNormalizeError::InvalidUri),
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionWire {
    line: u64,
    character: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeWire {
    start: PositionWire,
    end: PositionWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocationWire {
    uri: String,
    range: RangeWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocationLinkWire {
    #[serde(default)]
    origin_selection_range: Option<RangeWire>,
    target_uri: String,
    target_range: RangeWire,
    target_selection_range: RangeWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishDiagnosticsWire {
    uri: String,
    version: i32,
    diagnostics: Vec<DiagnosticWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticWire {
    range: RangeWire,
    #[serde(default)]
    severity: Option<u64>,
    #[serde(default)]
    code: Option<DiagnosticCodeWire>,
    #[serde(default)]
    source: Option<String>,
    message: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DiagnosticCodeWire {
    Integer(i64),
    String(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceEditWire {
    #[serde(default)]
    changes: Option<BTreeMap<String, Vec<TextEditWire>>>,
    #[serde(default)]
    document_changes: Option<Vec<DocumentChangeWire>>,
    #[serde(default)]
    change_annotations: BTreeMap<String, ChangeAnnotationWire>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DocumentChangeWire {
    Text(TextDocumentEditWire),
    Create(CreateFileWire),
    Rename(RenameFileWire),
    Delete(DeleteFileWire),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TextDocumentEditWire {
    text_document: OptionalVersionedDocumentWire,
    edits: Vec<TextEditWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalVersionedDocumentWire {
    uri: String,
    #[serde(default)]
    version: VersionField,
}

#[derive(Clone, Copy, Debug, Default)]
enum VersionField {
    #[default]
    Missing,
    Null,
    Number(i32),
}

impl<'de> Deserialize<'de> for VersionField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<i32>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Number(value),
            None => Self::Null,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TextEditWire {
    range: RangeWire,
    new_text: String,
    #[serde(default)]
    annotation_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangeAnnotationWire {
    label: String,
    #[serde(default)]
    needs_confirmation: Option<bool>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateFileWire {
    #[serde(rename = "kind")]
    _kind: CreateKind,
    uri: String,
    #[serde(default)]
    options: Option<CreateFileOptionsWire>,
    #[serde(default)]
    annotation_id: Option<String>,
}

#[derive(Deserialize)]
enum CreateKind {
    #[serde(rename = "create")]
    Create,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateFileOptionsWire {
    #[serde(default)]
    overwrite: Option<bool>,
    #[serde(default)]
    ignore_if_exists: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenameFileWire {
    #[serde(rename = "kind")]
    _kind: RenameKind,
    old_uri: String,
    new_uri: String,
    #[serde(default)]
    options: Option<RenameFileOptionsWire>,
    #[serde(default)]
    annotation_id: Option<String>,
}

#[derive(Deserialize)]
enum RenameKind {
    #[serde(rename = "rename")]
    Rename,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenameFileOptionsWire {
    #[serde(default)]
    overwrite: Option<bool>,
    #[serde(default)]
    ignore_if_exists: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteFileWire {
    #[serde(rename = "kind")]
    _kind: DeleteKind,
    uri: String,
    #[serde(default)]
    options: Option<DeleteFileOptionsWire>,
    #[serde(default)]
    annotation_id: Option<String>,
}

#[derive(Deserialize)]
enum DeleteKind {
    #[serde(rename = "delete")]
    Delete,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteFileOptionsWire {
    #[serde(default)]
    recursive: Option<bool>,
    #[serde(default)]
    ignore_if_not_exists: Option<bool>,
}

enum ParsedChange {
    Text(String, VersionField, Vec<TextEditWire>),
    Create(CreateFileWire),
    Rename(RenameFileWire),
    Delete(DeleteFileWire),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LspNormalizeError {
    InvalidRoot,
    InvalidSnapshot,
    InvalidUri,
    OutsideWorkspace,
    DuplicatePath(String),
    UnknownPath(String),
    UntrackedDocument(String),
    PathAlreadyExists(String),
    DocumentNotOpen,
    MissingDocumentVersion,
    StaleDocumentVersion,
    StaleDocumentEpoch,
    FutureDocumentVersion,
    StaleWorkspaceRevision,
    ServerMismatch,
    PositionEncodingMismatch,
    ServerErrorResponse,
    UnsupportedMethod,
    UnsupportedSemantics,
    UnsupportedEncoding,
    MalformedPayload,
    MalformedRange,
    AmbiguousOrdering,
    OverlappingEdits,
    UnknownAnnotation,
    UnsavedDocument,
    NoEffectiveChanges,
    LimitExceeded,
    EditIr(String),
}

impl fmt::Display for LspNormalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "LSP normalization rejected: {self:?}")
    }
}

impl std::error::Error for LspNormalizeError {}

impl From<crate::workspace::edit::ir::IrError> for LspNormalizeError {
    fn from(error: crate::workspace::edit::ir::IrError) -> Self {
        Self::EditIr(error.to_string())
    }
}

impl From<crate::workspace::edit::normalize::NormalizeError> for LspNormalizeError {
    fn from(error: crate::workspace::edit::normalize::NormalizeError) -> Self {
        Self::EditIr(error.to_string())
    }
}
