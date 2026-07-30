#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{fs, path::PathBuf};

use kit::{
    domain::{
        events::ContentDigest,
        ids::{PrincipalId, ProcessId, ProjectId, WorkspaceId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    },
    executor::profile::{
        Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
    },
    test_support::validate_edit_with_hook,
    verify::lsp::{
        facts::{
            ClassifiedRepositoryFact, FactLimits, LspNormalizeError, LspWorkspaceSnapshot,
            OpenDocument, RepositoryFactClassification, RepositoryFactProvenance, SnapshotFile,
            normalize_live_diagnostics, normalize_semantic_locations, normalize_workspace_edit,
        },
        session::{
            AcceptedNotification, AcceptedResponse, CodecLimits, DocumentVersion,
            ExecutionProfileIdentity, LaunchRequest, LspCodec, LspSessionManager,
            NotificationDisposition, OwnedLspLauncher, OwnedLspTransport, PositionEncoding,
            ResponseDisposition, RevisionPolicy, SendContext, ServerIdentity, SessionLimits,
            SessionPurpose, SessionScope, TransportError,
        },
    },
    workspace::{
        edit::ir::{EditIr, EditLimits, EditOperation},
        index::meta::{IndexOptions, MetadataIndex},
        revision::{ManagedWorkspace, RevisionId},
        syntax::SyntaxIndex,
    },
};
use serde_json::{Value, json};
use url::Url;

struct Fixture {
    parent: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 12];
        getrandom::fill(&mut random).unwrap();
        let parent = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-lsp-facts-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let root = parent.join("workspace");
        fs::create_dir_all(&root).unwrap();
        Self { parent, root }
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn uri(&self, path: &str) -> String {
        Url::from_file_path(self.root.join(path))
            .unwrap()
            .to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}

fn revision(byte: u8) -> RevisionId {
    RevisionId::parse(&format!("r:{}", format!("{byte:02x}").repeat(32))).unwrap()
}

fn digest(byte: u8) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", format!("{byte:02x}").repeat(32))).unwrap()
}

fn content_digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", blake3::hash(bytes).to_hex())).unwrap()
}

fn server() -> ServerIdentity {
    ServerIdentity {
        server_artifact: digest(1),
        configuration: digest(2),
    }
}

fn snapshot(
    fixture: &Fixture,
    revision: RevisionId,
    encoding: PositionEncoding,
    files: &[(&str, &str)],
    open: &[(&str, i32, &str)],
) -> LspWorkspaceSnapshot {
    snapshot_with_limits(
        fixture,
        revision,
        1,
        encoding,
        files,
        open,
        EditLimits::default(),
        FactLimits::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn snapshot_with_limits(
    fixture: &Fixture,
    revision: RevisionId,
    document_epoch: u64,
    encoding: PositionEncoding,
    files: &[(&str, &str)],
    open: &[(&str, i32, &str)],
    edit_limits: EditLimits,
    fact_limits: FactLimits,
) -> LspWorkspaceSnapshot {
    LspWorkspaceSnapshot::new(
        fixture.root.clone(),
        revision,
        document_epoch,
        files
            .iter()
            .map(|(path, text)| SnapshotFile::new(*path, text.as_bytes().to_vec(), false))
            .collect(),
        open.iter()
            .map(|(path, version, text)| {
                OpenDocument::new(
                    fixture.uri(path),
                    DocumentVersion::new(*version),
                    (*text).to_owned(),
                )
            })
            .collect(),
        server(),
        encoding,
        edit_limits,
        fact_limits,
    )
    .unwrap()
}

struct Launcher;

struct Transport {
    claim: ProcessClaim,
}

impl OwnedLspLauncher for Launcher {
    type Transport = Transport;

    fn launch(&mut self, request: LaunchRequest<'_>) -> Result<Self::Transport, TransportError> {
        Ok(Transport {
            claim: ProcessClaim::new(
                ProcessId::generate().unwrap(),
                ProcessOwnership::DaemonService(request.service.id),
            ),
        })
    }
}

impl OwnedLspTransport for Transport {
    fn claim(&self) -> ProcessClaim {
        self.claim
    }

    fn initialize(
        &mut self,
        _: &[u8],
        _: CodecLimits,
        _: SendContext,
    ) -> Result<(), TransportError> {
        Ok(())
    }

    fn send_frame(&mut self, _: &[u8], _: SendContext) -> Result<(), TransportError> {
        Ok(())
    }

    fn receive_frame(&mut self, _: CodecLimits, _: SendContext) -> Result<Vec<u8>, TransportError> {
        Err(TransportError::ReadFailed)
    }

    fn close_and_reap(&mut self, context: SendContext) -> Result<(), TransportError> {
        if context.remaining().is_zero() {
            Err(TransportError::CloseOrReapDeadlineExceeded)
        } else {
            Ok(())
        }
    }
}

fn profile() -> ExecutionProfileIdentity {
    #[cfg(target_os = "linux")]
    let platform = Platform::Linux;
    #[cfg(target_os = "macos")]
    let platform = Platform::MacOs;
    let profile = ExecutorProfile::new(ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        platform,
        Architecture::Aarch64,
        ResourceLimits::new(
            60_000,
            1024 * 1024 * 1024,
            64,
            64 * 1024 * 1024,
            1024 * 1024 * 1024,
            1024 * 1024 * 1024,
            64 * 1024 * 1024,
            60_000,
        ),
    ))
    .unwrap();
    ExecutionProfileIdentity::from_profile(&profile)
}

fn accepted(
    uri: &str,
    revision: RevisionId,
    version: i32,
    text: &str,
    method: &str,
    encoding: PositionEncoding,
    result: Value,
) -> AcceptedResponse {
    let mut manager = LspSessionManager::new(Launcher, SessionLimits::default()).unwrap();
    let service = manager
        .open(
            SessionScope {
                principal_id: PrincipalId::generate().unwrap(),
                project_id: ProjectId::generate().unwrap(),
                workspace_id: WorkspaceId::generate().unwrap(),
                canonical_root_identity: digest(3),
                purpose: SessionPurpose::Live,
                revision_policy: RevisionPolicy::ManagedLive,
                server: server(),
                position_encoding: encoding,
                execution_profile: profile(),
            },
            revision,
        )
        .unwrap();
    manager
        .open_document(
            service,
            uri.to_owned(),
            DocumentVersion::new(version),
            text.to_owned(),
        )
        .unwrap();
    let params = if matches!(
        method,
        "textDocument/declaration"
            | "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/implementation"
            | "textDocument/references"
    ) {
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 0}
        })
    } else {
        json!({})
    };
    let token = manager
        .request(
            service,
            revision,
            uri,
            method,
            params,
            manager.now_tick() + 10_000,
        )
        .unwrap();
    let frame = LspCodec::encode(
        &json!({"jsonrpc":"2.0","id":token.request_id.get(),"result":result}),
        SessionLimits::default().codec,
    )
    .unwrap();
    let ResponseDisposition::Accepted(accepted) = manager
        .receive_captured_response(service, &token, &frame)
        .unwrap()
    else {
        panic!("current response was not accepted");
    };
    assert_eq!(accepted.token(), &token);
    manager.shutdown().unwrap();
    accepted
}

fn accepted_notification(
    uri: &str,
    revision: RevisionId,
    version: i32,
    text: &str,
    encoding: PositionEncoding,
    params: Value,
) -> AcceptedNotification {
    let mut manager = LspSessionManager::new(Launcher, SessionLimits::default()).unwrap();
    let service = manager
        .open(
            SessionScope {
                principal_id: PrincipalId::generate().unwrap(),
                project_id: ProjectId::generate().unwrap(),
                workspace_id: WorkspaceId::generate().unwrap(),
                canonical_root_identity: digest(3),
                purpose: SessionPurpose::Live,
                revision_policy: RevisionPolicy::ManagedLive,
                server: server(),
                position_encoding: encoding,
                execution_profile: profile(),
            },
            revision,
        )
        .unwrap();
    manager
        .open_document(
            service,
            uri.to_owned(),
            DocumentVersion::new(version),
            text.to_owned(),
        )
        .unwrap();
    let generation = manager.snapshot(service).unwrap().generation;
    let frame = LspCodec::encode(
        &json!({
            "jsonrpc":"2.0",
            "method":"textDocument/publishDiagnostics",
            "params":params
        }),
        SessionLimits::default().codec,
    )
    .unwrap();
    let NotificationDisposition::Accepted(accepted) = manager
        .receive_notification(service, generation, &frame)
        .unwrap()
    else {
        panic!("current notification was not accepted");
    };
    manager.shutdown().unwrap();
    accepted
}

fn accepted_edit(
    uri: &str,
    revision: RevisionId,
    version: i32,
    text: &str,
    encoding: PositionEncoding,
    edit: Value,
) -> AcceptedResponse {
    accepted(
        uri,
        revision,
        version,
        text,
        "textDocument/rename",
        encoding,
        edit,
    )
}

#[test]
fn real_syntax_corpus_has_syntactic_tree_sitter_facts_and_zero_semantic_facts() {
    let fixture = Fixture::new();
    fixture.write("lib.rs", b"struct Item; impl Item { fn run(&self) {} }\n");
    let workspace = ManagedWorkspace::open(&fixture.root).unwrap();
    let revision = workspace.current_revision().unwrap();
    let mut syntax = SyntaxIndex::new();
    let index = MetadataIndex::build_with_syntax(
        &workspace,
        revision.id(),
        &IndexOptions::default(),
        &mut syntax,
    )
    .unwrap();
    let mut total = 0;
    let mut semantic = 0;
    for record in index
        .entries()
        .iter()
        .flat_map(|entry| entry.syntax_records.iter())
    {
        for fact in [
            record.qualified_name() as &dyn ClassifiedRepositoryFact,
            record.display_name(),
            record.kind(),
            record.signature(),
            record.declaration(),
        ] {
            total += 1;
            semantic +=
                usize::from(fact.classification() == RepositoryFactClassification::Semantic);
            assert_eq!(
                fact.repository_provenance(),
                RepositoryFactProvenance::TreeSitter
            );
        }
    }
    assert!(total > 0);
    assert_eq!(semantic, 0);
}

#[test]
fn semantic_locations_require_the_exact_accepted_current_fence() {
    let fixture = Fixture::new();
    fixture.write("main.rs", b"fn main() {}\n");
    let uri = fixture.uri("main.rs");
    let current = revision(10);
    let response = accepted(
        &uri,
        current,
        7,
        "fn main() {}\n",
        "textDocument/definition",
        PositionEncoding::Utf16,
        json!({"uri":uri,"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}}),
    );
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf16,
        &[("main.rs", "fn main() {}\n")],
        &[("main.rs", 7, "fn main() {}\n")],
    );
    let facts = normalize_semantic_locations(&snapshot, &response).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].range().start(), 3);
    assert_eq!(facts[0].range().end(), 7);
    assert_eq!(facts[0].origin_point(), 0);
    assert_eq!(
        (
            facts[0].origin_range().start(),
            facts[0].origin_range().end()
        ),
        (0, 0)
    );
    assert_eq!(
        facts[0].classification(),
        RepositoryFactClassification::Semantic
    );
    assert_eq!(
        facts[0].repository_provenance(),
        RepositoryFactProvenance::Lsp
    );

    let link_response = accepted(
        &uri,
        current,
        7,
        "fn main() {}\n",
        "textDocument/implementation",
        PositionEncoding::Utf16,
        json!([{
            "originSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":7}},
            "targetUri":uri,
            "targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},
            "targetSelectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}
        }]),
    );
    let links = normalize_semantic_locations(&snapshot, &link_response).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target_range().unwrap().start(), 0);
    assert_eq!(links[0].origin_range().end(), 7);

    let mismatched_link = accepted(
        &uri,
        current,
        7,
        "fn main() {}\n",
        "textDocument/implementation",
        PositionEncoding::Utf16,
        json!([{
            "originSelectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}},
            "targetUri":uri,
            "targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},
            "targetSelectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}
        }]),
    );
    assert_eq!(
        normalize_semantic_locations(&snapshot, &mismatched_link),
        Err(LspNormalizeError::MalformedRange)
    );

    let stale = self::snapshot(
        &fixture,
        revision(11),
        PositionEncoding::Utf16,
        &[("main.rs", "fn main() {}\n")],
        &[("main.rs", 7, "fn main() {}\n")],
    );
    assert_eq!(
        normalize_semantic_locations(&stale, &response),
        Err(LspNormalizeError::StaleWorkspaceRevision)
    );
}

#[test]
fn live_diagnostics_require_open_exact_version_and_revision() {
    let fixture = Fixture::new();
    fixture.write("main.rs", b"let x = 1;\n");
    let current = revision(20);
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("main.rs", "let x = 1;\n")],
        &[("main.rs", 4, "let x = 1;\n")],
    );
    let payload = json!({
        "uri": fixture.uri("main.rs"),
        "version": 4,
        "diagnostics": [{
            "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}},
            "severity":2,"code":"unused","source":"test","message":"unused binding"
        }]
    });
    let notification = accepted_notification(
        &fixture.uri("main.rs"),
        current,
        4,
        "let x = 1;\n",
        PositionEncoding::Utf8,
        payload.clone(),
    );
    let diagnostics = normalize_live_diagnostics(&snapshot, &notification).unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message(), "unused binding");
    assert_eq!(diagnostics[0].provenance().revision(), current);

    for (version, expected) in [
        (3, LspNormalizeError::StaleDocumentVersion),
        (5, LspNormalizeError::FutureDocumentVersion),
    ] {
        let mut versioned = payload.clone();
        versioned["version"] = version.into();
        let accepted = accepted_notification(
            &fixture.uri("main.rs"),
            current,
            version,
            "let x = 1;\n",
            PositionEncoding::Utf8,
            versioned,
        );
        assert_eq!(
            normalize_live_diagnostics(&snapshot, &accepted),
            Err(expected)
        );
    }
    let stale_snapshot = self::snapshot(
        &fixture,
        revision(21),
        PositionEncoding::Utf8,
        &[("main.rs", "let x = 1;\n")],
        &[("main.rs", 4, "let x = 1;\n")],
    );
    assert_eq!(
        normalize_live_diagnostics(&stale_snapshot, &notification),
        Err(LspNormalizeError::StaleWorkspaceRevision)
    );
}

#[test]
fn workspace_edit_lowers_all_four_ops_roundtrips_and_writes_nothing() {
    let fixture = Fixture::new();
    for (path, text) in [("a.rs", "abc\n"), ("b.rs", "move\n"), ("c.rs", "delete\n")] {
        fixture.write(path, text.as_bytes());
    }
    let current = revision(30);
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "abc\n"), ("b.rs", "move\n"), ("c.rs", "delete\n")],
        &[("a.rs", 2, "abc\n")],
    );
    let edit = json!({
        "documentChanges": [
            {"textDocument":{"uri":fixture.uri("a.rs"),"version":2},"edits":[{
                "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},
                "newText":"B","annotationId":"safe"
            }]},
            {"kind":"create","uri":fixture.uri("new.rs"),"options":{"overwrite":false,"ignoreIfExists":false}},
            {"kind":"rename","oldUri":fixture.uri("b.rs"),"newUri":fixture.uri("moved.rs")},
            {"kind":"delete","uri":fixture.uri("c.rs"),"options":{"recursive":false,"ignoreIfNotExists":false}}
        ],
        "changeAnnotations":{"safe":{"label":"safe edit","needsConfirmation":false}}
    });
    let response = accepted_edit(
        &fixture.uri("a.rs"),
        current,
        2,
        "abc\n",
        PositionEncoding::Utf8,
        edit,
    );
    assert_eq!(response.token().document_version, DocumentVersion::new(2));
    let ir = normalize_workspace_edit(&snapshot, &response).unwrap();
    assert_eq!(ir.operations().len(), 4);
    let EditOperation::ReplaceRange {
        path,
        base_digest,
        range,
        expected,
        replacement,
        ..
    } = ir.operations()[0].operation()
    else {
        panic!("expected replacement");
    };
    assert_eq!(path.as_str(), "a.rs");
    assert_eq!(base_digest, &content_digest(b"abc\n"));
    assert_eq!((range.start, range.end), (0, 4));
    assert_eq!(expected.render(), b"abc\n");
    assert_eq!(replacement.render(), b"aBc\n");
    let EditOperation::AddFile { path, content, .. } = ir.operations()[1].operation() else {
        panic!("expected create");
    };
    assert_eq!(path.as_str(), "new.rs");
    assert!(content.render().is_empty());
    let EditOperation::MoveFile {
        from,
        to,
        base_digest,
    } = ir.operations()[2].operation()
    else {
        panic!("expected rename");
    };
    assert_eq!((from.as_str(), to.as_str()), ("b.rs", "moved.rs"));
    assert_eq!(base_digest, &content_digest(b"move\n"));
    let EditOperation::DeleteFile { path, base_digest } = ir.operations()[3].operation() else {
        panic!("expected delete");
    };
    assert_eq!(path.as_str(), "c.rs");
    assert_eq!(base_digest, &content_digest(b"delete\n"));
    assert_eq!(
        EditIr::from_canonical_bytes(&ir.canonical_bytes(), EditLimits::default()).unwrap(),
        ir
    );
    assert_eq!(fs::read(fixture.root.join("a.rs")).unwrap(), b"abc\n");
    assert_eq!(fs::read(fixture.root.join("b.rs")).unwrap(), b"move\n");
    assert_eq!(fs::read(fixture.root.join("c.rs")).unwrap(), b"delete\n");
    assert!(!fixture.root.join("new.rs").exists());
    assert!(!fixture.root.join("moved.rs").exists());
}

#[test]
fn workspace_edit_preserves_resource_first_document_change_order() {
    let fixture = Fixture::new();
    fixture.write("existing.rs", b"old\n");
    let current = revision(31);
    let uri = fixture.uri("existing.rs");
    let response = accepted_edit(
        &uri,
        current,
        4,
        "old\n",
        PositionEncoding::Utf8,
        json!({"documentChanges":[
            {"kind":"create","uri":fixture.uri("created.rs")},
            {"textDocument":{"uri":uri,"version":4},"edits":[{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
                "newText":"new"
            }]}
        ]}),
    );
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("existing.rs", "old\n")],
        &[("existing.rs", 4, "old\n")],
    );

    let ir = normalize_workspace_edit(&snapshot, &response).unwrap();
    assert_eq!(ir.operations().len(), 2);
    let EditOperation::AddFile { path, content, .. } = ir.operations()[0].operation() else {
        panic!("expected resource operation first");
    };
    assert_eq!(path.as_str(), "created.rs");
    assert_eq!(content.render(), b"");
    let EditOperation::ReplaceRange {
        path,
        expected,
        replacement,
        ..
    } = ir.operations()[1].operation()
    else {
        panic!("expected text operation second");
    };
    assert_eq!(path.as_str(), "existing.rs");
    assert_eq!(expected.render(), b"old\n");
    assert_eq!(replacement.render(), b"new\n");
}

#[test]
fn utf8_utf16_utf32_and_crlf_positions_are_exact() {
    let fixture = Fixture::new();
    fixture.write("unicode.rs", "a😀z\r\nnext".as_bytes());
    let current = revision(40);
    for (encoding, start, end) in [
        (PositionEncoding::Utf8, 1, 5),
        (PositionEncoding::Utf16, 1, 3),
        (PositionEncoding::Utf32, 1, 2),
    ] {
        let uri = fixture.uri("unicode.rs");
        let response = accepted(
            &uri,
            current,
            1,
            "a😀z\r\nnext",
            "textDocument/declaration",
            encoding,
            json!({"uri":uri,"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}}}),
        );
        let snapshot = snapshot(
            &fixture,
            current,
            encoding,
            &[("unicode.rs", "a😀z\r\nnext")],
            &[("unicode.rs", 1, "a😀z\r\nnext")],
        );
        let fact = normalize_semantic_locations(&snapshot, &response).unwrap();
        assert_eq!((fact[0].range().start(), fact[0].range().end()), (1, 5));
    }

    for (encoding, middle) in [(PositionEncoding::Utf8, 2), (PositionEncoding::Utf16, 2)] {
        let uri = fixture.uri("unicode.rs");
        let response = accepted(
            &uri,
            current,
            1,
            "a😀z\r\nnext",
            "textDocument/declaration",
            encoding,
            json!({"uri":uri,"range":{"start":{"line":0,"character":middle},"end":{"line":0,"character":middle}}}),
        );
        let snapshot = snapshot(
            &fixture,
            current,
            encoding,
            &[("unicode.rs", "a😀z\r\nnext")],
            &[("unicode.rs", 1, "a😀z\r\nnext")],
        );
        assert_eq!(
            normalize_semantic_locations(&snapshot, &response),
            Err(LspNormalizeError::MalformedRange)
        );
    }
}

#[test]
fn text_edit_order_overlap_identity_and_fences_are_explicit() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"abcdef");
    let current = revision(50);
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "abcdef")],
        &[("a.rs", 3, "abcdef")],
    );
    let ordered = json!({"changes":{fixture.uri("a.rs"):[
        {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}},"newText":"E"},
        {"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},"newText":"B"}
    ]}});
    let ir = normalize_workspace_edit(
        &snapshot,
        &accepted_edit(
            &fixture.uri("a.rs"),
            current,
            3,
            "abcdef",
            PositionEncoding::Utf8,
            ordered,
        ),
    )
    .unwrap();
    let EditOperation::ReplaceRange { replacement, .. } = ir.operations()[0].operation() else {
        panic!("expected replacement");
    };
    assert_eq!(replacement.render(), b"aBcdEf");

    let identity = json!({"changes":{fixture.uri("a.rs"):[
        {"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},"newText":"b"}
    ]}});
    let identity_response = accepted_edit(
        &fixture.uri("a.rs"),
        current,
        3,
        "abcdef",
        PositionEncoding::Utf8,
        identity,
    );
    assert_eq!(
        normalize_workspace_edit(&snapshot, &identity_response),
        Err(LspNormalizeError::NoEffectiveChanges)
    );
    let overlap = json!({"changes":{fixture.uri("a.rs"):[
        {"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"x"},
        {"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":4}},"newText":"y"}
    ]}});
    assert_eq!(
        normalize_workspace_edit(
            &snapshot,
            &accepted_edit(
                &fixture.uri("a.rs"),
                current,
                3,
                "abcdef",
                PositionEncoding::Utf8,
                overlap,
            ),
        ),
        Err(LspNormalizeError::OverlappingEdits)
    );
    let rebased = self::snapshot(
        &fixture,
        revision(51),
        PositionEncoding::Utf8,
        &[("a.rs", "abcdef")],
        &[("a.rs", 3, "abcdef")],
    );
    assert_eq!(
        normalize_workspace_edit(&rebased, &identity_response),
        Err(LspNormalizeError::StaleWorkspaceRevision)
    );

    let wrong_version = json!({"documentChanges":[{
        "textDocument":{"uri":fixture.uri("a.rs"),"version":2},"edits":[{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"A"
        }]
    }]});
    assert_eq!(
        normalize_workspace_edit(
            &snapshot,
            &accepted_edit(
                &fixture.uri("a.rs"),
                current,
                3,
                "abcdef",
                PositionEncoding::Utf8,
                wrong_version,
            )
        ),
        Err(LspNormalizeError::StaleDocumentVersion)
    );
}

#[test]
fn malformed_annotations_resource_options_and_unrepresentable_order_reject() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"abc");
    let current = revision(60);
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "abc")],
        &[("a.rs", 1, "abc")],
    );
    let cases = [
        json!({"changes":{fixture.uri("a.rs"):[]},"documentChanges":[{"kind":"create","uri":fixture.uri("b.rs") }]}),
        json!({"documentChanges":[{"kind":"create","uri":fixture.uri("b.rs"),"options":{"overwrite":true}}]}),
        json!({"documentChanges":[{"kind":"delete","uri":fixture.uri("a.rs"),"options":{"recursive":true}}]}),
        json!({"documentChanges":[{"kind":"create","uri":fixture.uri("b.rs"),"annotationId":"missing"}]}),
        json!({"documentChanges":[{"kind":"create","uri":fixture.uri("b.rs"),"annotationId":"ask"}],"changeAnnotations":{"ask":{"label":"ask","needsConfirmation":true}}}),
        json!({"documentChanges":[{"textDocument":{"uri":fixture.uri("a.rs")},"edits":[]}]}),
        json!({"documentChanges":[{"kind":"rename","oldUri":fixture.uri("a.rs"),"newUri":fixture.uri("b.rs")},{"kind":"delete","uri":fixture.uri("a.rs")}]}),
    ];
    for value in cases {
        let response = accepted_edit(
            &fixture.uri("a.rs"),
            current,
            1,
            "abc",
            PositionEncoding::Utf8,
            value.clone(),
        );
        assert!(
            normalize_workspace_edit(&snapshot, &response).is_err(),
            "accepted unsupported edit: {value}"
        );
    }
}

#[test]
fn outside_uri_corpus_rejects_whole_edit_and_symlink_defense_keeps_sentinel() {
    let fixture = Fixture::new();
    fixture.write("inside.rs", b"inside");
    let outside = fixture.parent.join("outside.rs");
    fs::write(&outside, b"sentinel").unwrap();
    let current = revision(70);
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("inside.rs", "inside")],
        &[("inside.rs", 1, "inside")],
    );
    let outside_uri = Url::from_file_path(&outside).unwrap().to_string();
    let root = fixture.root.to_str().unwrap();
    let hostile = [
        outside_uri,
        format!("file://evil{root}/inside.rs"),
        format!("file://{root}/../outside.rs"),
        format!("file://{root}/%2e%2e/outside.rs"),
        format!("file://{root}/dir%2Foutside.rs"),
        format!("file://{root}/dir%5Coutside.rs"),
        format!("file://{root}/inside.rs?query=1"),
        format!("file://{root}/inside.rs#fragment"),
        format!("file://user@localhost{root}/inside.rs"),
        format!("file://{root}/%00inside.rs"),
    ];
    for uri in hostile {
        let edit = json!({"changes":{
            fixture.uri("inside.rs"):[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"I"}],
            uri:[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"owned"}]
        }});
        let response = accepted_edit(
            &fixture.uri("inside.rs"),
            current,
            1,
            "inside",
            PositionEncoding::Utf8,
            edit,
        );
        assert!(
            normalize_workspace_edit(&snapshot, &response).is_err(),
            "accepted hostile URI"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
        assert_eq!(fs::read(fixture.root.join("inside.rs")).unwrap(), b"inside");
    }

    fixture.write("dir/victim.rs", b"workspace");
    let outside_dir = fixture.parent.join("outside-dir");
    fs::create_dir(&outside_dir).unwrap();
    fs::write(outside_dir.join("victim.rs"), b"outside").unwrap();
    let managed = ManagedWorkspace::open(&fixture.root).unwrap();
    let actual_revision = managed.current_revision().unwrap().id();
    let symlink_snapshot = self::snapshot(
        &fixture,
        actual_revision,
        PositionEncoding::Utf8,
        &[("dir/victim.rs", "workspace")],
        &[("dir/victim.rs", 1, "workspace")],
    );
    let response = accepted_edit(
        &fixture.uri("dir/victim.rs"),
        actual_revision,
        1,
        "workspace",
        PositionEncoding::Utf8,
        json!({"changes":{fixture.uri("dir/victim.rs"):[{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":9}},"newText":"changed"
        }]}}),
    );
    let ir = normalize_workspace_edit(&symlink_snapshot, &response).unwrap();
    assert_eq!(ir.operations().len(), 1);
    let original = fixture.root.join("dir");
    let saved = fixture.root.join("saved-dir");
    let mut swapped = false;
    assert!(
        validate_edit_with_hook(&managed, &ir, EditLimits::default(), |point, _| {
            if point == "guard-acquired" && !swapped {
                swapped = true;
                fs::rename(&original, &saved).unwrap();
                std::os::unix::fs::symlink(&outside_dir, &original).unwrap();
            }
        })
        .is_err()
    );
    assert!(swapped);
    assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
    assert_eq!(fs::read(outside_dir.join("victim.rs")).unwrap(), b"outside");
    assert_eq!(fs::read(saved.join("victim.rs")).unwrap(), b"workspace");
}

#[test]
fn tracked_unsaved_text_rejects_live_edit_ir() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"base");
    let current = revision(80);
    let snapshot = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "base")],
        &[("a.rs", 2, "dirty")],
    );
    let edit = json!({"documentChanges":[{
        "textDocument":{"uri":fixture.uri("a.rs"),"version":2},
        "edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"D"}]
    }]});
    assert_eq!(
        normalize_workspace_edit(
            &snapshot,
            &accepted_edit(
                &fixture.uri("a.rs"),
                current,
                2,
                "dirty",
                PositionEncoding::Utf8,
                edit,
            ),
        ),
        Err(LspNormalizeError::UnsavedDocument)
    );
}

#[test]
fn dirty_open_documents_reject_rename_delete_and_create_destination() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"base");
    let current = revision(81);
    let dirty = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "base")],
        &[("a.rs", 2, "dirty")],
    );
    for edit in [
        json!({"documentChanges":[{"kind":"rename","oldUri":fixture.uri("a.rs"),"newUri":fixture.uri("b.rs")}]}),
        json!({"documentChanges":[{"kind":"delete","uri":fixture.uri("a.rs")}]}),
    ] {
        let response = accepted_edit(
            &fixture.uri("a.rs"),
            current,
            2,
            "dirty",
            PositionEncoding::Utf8,
            edit,
        );
        assert_eq!(
            normalize_workspace_edit(&dirty, &response),
            Err(LspNormalizeError::UnsavedDocument)
        );
    }

    let clean = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "base")],
        &[("a.rs", 2, "base")],
    );
    let create = accepted_edit(
        &fixture.uri("a.rs"),
        current,
        2,
        "base",
        PositionEncoding::Utf8,
        json!({"documentChanges":[{"kind":"create","uri":fixture.uri("a.rs")}]}),
    );
    assert!(matches!(
        normalize_workspace_edit(&clean, &create),
        Err(LspNormalizeError::PathAlreadyExists(_))
    ));
}

#[test]
fn crlf_delimiters_epoch_and_position_work_are_fenced() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"a\r\nb");
    let current = revision(82);
    let uri = fixture.uri("a.rs");
    let response = accepted(
        &uri,
        current,
        1,
        "a\r\nb",
        "textDocument/definition",
        PositionEncoding::Utf8,
        json!({"uri":uri,"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}}}),
    );
    let exact = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "a\r\nb")],
        &[("a.rs", 1, "a\r\nb")],
    );
    let fact = normalize_semantic_locations(&exact, &response).unwrap();
    assert_eq!((fact[0].range().start(), fact[0].range().end()), (3, 4));

    for (line, character) in [(0, 2), (1, 2)] {
        let rejected = accepted(
            &fixture.uri("a.rs"),
            current,
            1,
            "a\r\nb",
            "textDocument/definition",
            PositionEncoding::Utf8,
            json!({"uri":fixture.uri("a.rs"),"range":{"start":{"line":line,"character":character},"end":{"line":line,"character":character}}}),
        );
        assert_eq!(
            normalize_semantic_locations(&exact, &rejected),
            Err(LspNormalizeError::MalformedRange)
        );
    }
    let stale_epoch = snapshot_with_limits(
        &fixture,
        current,
        2,
        PositionEncoding::Utf8,
        &[("a.rs", "a\r\nb")],
        &[("a.rs", 1, "a\r\nb")],
        EditLimits::default(),
        FactLimits::default(),
    );
    assert_eq!(
        normalize_semantic_locations(&stale_epoch, &response),
        Err(LspNormalizeError::StaleDocumentEpoch)
    );

    let tiny_work = snapshot_with_limits(
        &fixture,
        current,
        1,
        PositionEncoding::Utf8,
        &[("a.rs", "a\r\nb")],
        &[("a.rs", 1, "a\r\nb")],
        EditLimits::default(),
        FactLimits {
            max_position_work_bytes: 1,
            ..FactLimits::default()
        },
    );
    assert_eq!(
        normalize_semantic_locations(&tiny_work, &response),
        Err(LspNormalizeError::LimitExceeded)
    );
}

#[test]
fn retained_output_snapshot_and_raw_operation_budgets_are_finite() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"abcdef");
    fixture.write("b.rs", b"b");
    let current = revision(83);
    let long_path = format!("{}.rs", "x".repeat(180));
    fixture.write(&long_path, b"x");
    let long_uri = fixture.uri(&long_path);
    let long_origin = accepted(
        &long_uri,
        current,
        1,
        "x",
        "textDocument/definition",
        PositionEncoding::Utf8,
        json!({"uri":long_uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}),
    );
    let long_bounded = snapshot_with_limits(
        &fixture,
        current,
        1,
        PositionEncoding::Utf8,
        &[(long_path.as_str(), "x")],
        &[(long_path.as_str(), 1, "x")],
        EditLimits::default(),
        FactLimits {
            max_retained_output_bytes: 128,
            ..FactLimits::default()
        },
    );
    assert_eq!(
        normalize_semantic_locations(&long_bounded, &long_origin),
        Err(LspNormalizeError::LimitExceeded)
    );

    let uri = fixture.uri("a.rs");
    let many = accepted(
        &uri,
        current,
        1,
        "abcdef",
        "textDocument/references",
        PositionEncoding::Utf8,
        Value::Array(
            (0..100)
                .map(|_| json!({"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}))
                .collect(),
        ),
    );
    let bounded = snapshot_with_limits(
        &fixture,
        current,
        1,
        PositionEncoding::Utf8,
        &[("a.rs", "abcdef"), ("b.rs", "b")],
        &[("a.rs", 1, "abcdef")],
        EditLimits::default(),
        FactLimits {
            max_retained_output_bytes: 1_024,
            ..FactLimits::default()
        },
    );
    assert_eq!(
        normalize_semantic_locations(&bounded, &many),
        Err(LspNormalizeError::LimitExceeded)
    );
    let diagnostics = accepted_notification(
        &uri,
        current,
        1,
        "abcdef",
        PositionEncoding::Utf8,
        json!({
            "uri":uri,
            "version":1,
            "diagnostics":(0..100).map(|index| json!({
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "code":format!("code-{index}"),
                "source":"hostile-origin",
                "message":"m".repeat(128)
            })).collect::<Vec<_>>()
        }),
    );
    assert_eq!(
        normalize_live_diagnostics(&bounded, &diagnostics),
        Err(LspNormalizeError::LimitExceeded)
    );

    assert!(
        LspWorkspaceSnapshot::new(
            fixture.root.clone(),
            current,
            1,
            vec![
                SnapshotFile::new("a.rs", b"abcdef".to_vec(), false),
                SnapshotFile::new("b.rs", b"b".to_vec(), false),
            ],
            vec![
                OpenDocument::new(uri.clone(), DocumentVersion::new(1), "abcdef".to_owned()),
                OpenDocument::new(fixture.uri("b.rs"), DocumentVersion::new(1), "b".to_owned(),),
            ],
            server(),
            PositionEncoding::Utf8,
            EditLimits::default(),
            FactLimits {
                max_open_documents: 1,
                ..FactLimits::default()
            },
        )
        .is_err()
    );
    assert!(
        LspWorkspaceSnapshot::new(
            fixture.root.clone(),
            current,
            1,
            vec![SnapshotFile::new("a.rs", b"abcdef".to_vec(), false)],
            vec![OpenDocument::new(
                uri.clone(),
                DocumentVersion::new(1),
                "abcdef".to_owned(),
            )],
            server(),
            PositionEncoding::Utf8,
            EditLimits::default(),
            FactLimits {
                max_open_document_bytes: 1,
                ..FactLimits::default()
            },
        )
        .is_err()
    );

    let edit_limits = EditLimits {
        max_operations: 1,
        ..EditLimits::default()
    };
    let operation_bounded = snapshot_with_limits(
        &fixture,
        current,
        1,
        PositionEncoding::Utf8,
        &[("a.rs", "abcdef"), ("b.rs", "b")],
        &[("a.rs", 1, "abcdef")],
        edit_limits,
        FactLimits::default(),
    );
    let empty_groups = accepted_edit(
        &uri,
        current,
        1,
        "abcdef",
        PositionEncoding::Utf8,
        json!({"changes":{(uri.clone()):[],(fixture.uri("b.rs")):[]}}),
    );
    assert_eq!(
        normalize_workspace_edit(&operation_bounded, &empty_groups),
        Err(LspNormalizeError::LimitExceeded)
    );
}

#[test]
fn snapshot_preflights_newline_and_tiny_file_amplification() {
    let fixture = Fixture::new();
    let current = revision(85);
    let limits = FactLimits {
        max_document_bytes: 4 * 1024,
        max_workspace_bytes: 8 * 1024,
        ..FactLimits::default()
    };
    assert!(matches!(
        LspWorkspaceSnapshot::new(
            fixture.root.clone(),
            current,
            1,
            vec![SnapshotFile::new("lines.rs", vec![b'\n'; 1_024], false)],
            Vec::new(),
            server(),
            PositionEncoding::Utf8,
            EditLimits::default(),
            limits,
        ),
        Err(LspNormalizeError::LimitExceeded)
    ));

    let files = (0..40)
        .map(|index| SnapshotFile::new(format!("{index}.rs"), b"x".to_vec(), false))
        .collect();
    assert!(matches!(
        LspWorkspaceSnapshot::new(
            fixture.root.clone(),
            current,
            1,
            files,
            Vec::new(),
            server(),
            PositionEncoding::Utf8,
            EditLimits::default(),
            limits,
        ),
        Err(LspNormalizeError::LimitExceeded)
    ));
}

#[test]
fn workspace_edit_rejects_unrelated_method_and_precharges_whole_files() {
    let fixture = Fixture::new();
    fixture.write("a.rs", b"base");
    let current = revision(84);
    let uri = fixture.uri("a.rs");
    let edit = json!({"changes":{(uri.clone()):[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},"newText":"next"
    }]}});
    let unrelated = accepted(
        &uri,
        current,
        1,
        "base",
        "textDocument/hover",
        PositionEncoding::Utf8,
        edit.clone(),
    );
    let regular = snapshot(
        &fixture,
        current,
        PositionEncoding::Utf8,
        &[("a.rs", "base")],
        &[("a.rs", 1, "base")],
    );
    assert_eq!(
        normalize_workspace_edit(&regular, &unrelated),
        Err(LspNormalizeError::UnsupportedMethod)
    );

    let tiny_content = snapshot_with_limits(
        &fixture,
        current,
        1,
        PositionEncoding::Utf8,
        &[("a.rs", "base")],
        &[("a.rs", 1, "base")],
        EditLimits {
            max_content_bytes: 7,
            ..EditLimits::default()
        },
        FactLimits::default(),
    );
    let response = accepted_edit(&uri, current, 1, "base", PositionEncoding::Utf8, edit);
    assert_eq!(
        normalize_workspace_edit(&tiny_content, &response),
        Err(LspNormalizeError::LimitExceeded)
    );
}
