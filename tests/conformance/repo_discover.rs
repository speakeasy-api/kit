#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kit::{
    domain::secret::{SecretCustody, SecretLease},
    store::artifacts::{
        ArtifactClass, ArtifactDigest, ArtifactMetadata, ArtifactRetention, ArtifactStore,
        Reachability,
    },
    workspace::{
        index::meta::{IndexOptions, MetadataIndex},
        read::{
            ArtifactContext, ArtifactResolveOptions, Encoding, NewlineStyle, ReadError,
            ReadOptions, ReadPublishPoint, ReadRange, ReadRequest, WorkspaceArtifactHandle, read,
            read_projected, read_with_publish_hook, read_with_stage_hook, resolve_artifact,
        },
        revision::{ManagedWorkspace, RevisionError, RevisionOptions},
        search::discover::{DiscoverError, DiscoverKind, DiscoverOptions, DiscoverQuery, discover},
    },
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
    artifact_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-repo-discover-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        let artifact_path = root.join("artifacts");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&workspace_path).unwrap();
        Self {
            root,
            workspace_path,
            artifact_path,
        }
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.workspace_path.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn open(&self) -> ManagedWorkspace {
        ManagedWorkspace::open_with_options(
            &self.workspace_path,
            RevisionOptions {
                max_entries: 20_000,
                max_name_bytes: 16 * 1024 * 1024,
                max_bytes: 128 * 1024 * 1024,
                max_memory_bytes: 256 * 1024 * 1024,
                max_depth: 128,
                max_scan_time: Duration::from_secs(10),
                max_scan_attempts: 2,
                watcher_interval: Duration::from_millis(5),
                reconciliation_interval: Duration::from_secs(60),
                metadata_path: Some(self.root.join("revision.state")),
            },
        )
        .unwrap()
    }

    fn indexed(&self) -> (ManagedWorkspace, MetadataIndex) {
        let workspace = self.open();
        let revision = workspace.current_revision().unwrap();
        let index =
            MetadataIndex::build(&workspace, revision.id(), &IndexOptions::default()).unwrap();
        (workspace, index)
    }

    fn artifacts(&self) -> ArtifactStore {
        ArtifactStore::open(&self.artifact_path).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn query(term: &str) -> DiscoverQuery {
    DiscoverQuery {
        terms: vec![term.to_owned()],
        roots: Vec::new(),
        languages: Vec::new(),
    }
}

fn context(principal: &str) -> ArtifactContext {
    ArtifactContext {
        principal: principal.to_owned(),
        project: "project".to_owned(),
        retention: ArtifactRetention::Forever,
    }
}

fn legacy_v3_artifact(
    workspace: &ManagedWorkspace,
    store: &ArtifactStore,
    context: &ArtifactContext,
    request: &ReadRequest,
    payload: &[u8],
) -> WorkspaceArtifactHandle {
    let epoch = workspace.current_revision().unwrap().epoch().to_string();
    let revision = request.expected_revision.to_string();
    let path = request.path.as_os_str().as_encoded_bytes();
    let digest = blake3::hash(payload);
    let mut binding = blake3::Hasher::new();
    binding.update(b"kit-workspace-artifact-auth-v3\0");
    for value in [
        context.principal.as_bytes(),
        context.project.as_bytes(),
        epoch.as_bytes(),
        revision.as_bytes(),
        path,
    ] {
        binding.update(&(value.len() as u64).to_le_bytes());
        binding.update(value);
    }
    binding.update(&[0]);
    binding.update(&0_u64.to_le_bytes());
    binding.update(&0_u64.to_le_bytes());
    binding.update(&0_u64.to_le_bytes());
    binding.update(&(payload.len() as u64).to_le_bytes());
    binding.update(digest.as_bytes());

    let mut envelope = b"kit-workspace-artifact-v3\0".to_vec();
    for value in [epoch.as_bytes(), revision.as_bytes(), path] {
        envelope.extend_from_slice(&(value.len() as u64).to_le_bytes());
        envelope.extend_from_slice(value);
    }
    envelope.push(0);
    envelope.extend_from_slice(&0_u64.to_le_bytes());
    envelope.extend_from_slice(&0_u64.to_le_bytes());
    envelope.extend_from_slice(&0_u64.to_le_bytes());
    envelope.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    let media_type = b"application/octet-stream";
    envelope.extend_from_slice(&(media_type.len() as u64).to_le_bytes());
    envelope.extend_from_slice(media_type);
    envelope.extend_from_slice(binding.finalize().as_bytes());
    envelope.push(0);
    envelope.extend_from_slice(digest.as_bytes());
    envelope.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    envelope.extend_from_slice(payload);

    let staged = store
        .stage(
            &envelope,
            ArtifactMetadata::new(
                "application/vnd.kit.workspace-read-envelope",
                ArtifactClass::File,
                context.principal.clone(),
                context.project.clone(),
                context.retention,
                0,
            )
            .unwrap(),
        )
        .unwrap();
    let artifact_digest = staged.digest();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut committed = staged
        .promote_pending()
        .unwrap()
        .commit_unissued_before(deadline)
        .unwrap();
    committed.issue_workspace_before(deadline).unwrap();
    committed.finish().unwrap();

    let mut mask = blake3::Hasher::new();
    mask.update(b"kit-workspace-artifact-handle-v3\0");
    for value in [
        context.principal.as_bytes(),
        context.project.as_bytes(),
        revision.as_bytes(),
        path,
    ] {
        mask.update(&(value.len() as u64).to_le_bytes());
        mask.update(value);
    }
    mask.update(&[0]);
    mask.update(&0_u64.to_le_bytes());
    mask.update(&0_u64.to_le_bytes());
    let mut opaque = artifact_digest.as_bytes();
    for (byte, mask) in opaque.iter_mut().zip(mask.finalize().as_bytes()) {
        *byte ^= mask;
    }
    WorkspaceArtifactHandle {
        id: format!(
            "kit-workspace-artifact:v3:{}",
            opaque
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
        path: request.path.clone(),
        path_digest: ArtifactDigest::digest(path).to_string(),
    }
}

#[test]
fn projected_read_reports_projected_coordinates_without_false_gaps() {
    let fixture = Fixture::new();
    let source = b"left workspace-secret\n";
    fixture.write("secret.txt", source);
    let (workspace, index) = fixture.indexed();
    let revision = index.revision();
    let custody = SecretCustody::new([Arc::new(SecretLease::new("workspace-secret\n"))]);
    let response = read_projected(
        &workspace,
        &index,
        &fixture.artifacts(),
        &context("projected-read"),
        &ReadRequest {
            expected_revision: revision,
            path: PathBuf::from("secret.txt"),
            range: ReadRange::Full,
        },
        &ReadOptions::default(),
        &custody,
    )
    .unwrap();
    assert_eq!(response.content, b"left [REDACTED]");
    assert_eq!(response.byte_start, 0);
    assert_eq!(response.byte_end, response.content.len());
    assert_eq!(response.file_bytes, source.len());
    assert_eq!(response.source_offset, Some(0));
    assert_eq!(response.source_length, Some(source.len()));
    assert_eq!(
        response.source_digest.as_deref(),
        Some(ArtifactDigest::digest(source).to_string().as_str())
    );
    assert_eq!(response.source_file_size, Some(source.len()));
    assert_eq!(response.projected_offset, Some(0));
    assert_eq!(response.projected_length, Some(response.content.len()));
    assert_eq!(
        response.projected_digest.as_deref(),
        Some(
            ArtifactDigest::digest(&response.content)
                .to_string()
                .as_str()
        )
    );
    assert!(response.gap.is_none());
    assert!(!response.truncated);
    assert!(!response.final_newline);
    assert_eq!(
        response.result_bytes,
        response.to_canonical_json().unwrap().len()
    );
}

#[test]
fn projected_read_hides_secret_filenames_and_projects_artifact_path_metadata() {
    let fixture = Fixture::new();
    fixture.write("workspace-secret.bin", b"public\0binary");
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let custody = SecretCustody::new([Arc::new(SecretLease::new("workspace-secret"))]);
    let request = ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from("workspace-secret.bin"),
        range: ReadRange::Full,
    };
    let response = read_projected(
        &workspace,
        &index,
        &artifacts,
        &context("secret-filename"),
        &request,
        &ReadOptions::default(),
        &custody,
    )
    .unwrap();

    assert_eq!(response.path, Path::new("[REDACTED].bin"));
    let artifact = response.artifact.as_ref().unwrap();
    assert_eq!(artifact.path, Path::new("[REDACTED].bin"));
    assert_eq!(
        artifact.path_digest,
        ArtifactDigest::digest(artifact.path.as_os_str().as_encoded_bytes()).to_string()
    );
    assert!(
        !String::from_utf8_lossy(&response.to_canonical_json().unwrap())
            .contains("workspace-secret")
    );
    assert_eq!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &context("secret-filename"),
            &request,
            artifact,
            &ArtifactResolveOptions::default(),
        )
        .unwrap(),
        b"public\0binary"
    );
}

#[test]
fn projected_read_prevents_path_and_content_fragments_from_reconstructing() {
    let fixture = Fixture::new();
    fixture.write("split-", b"secret");
    let (workspace, index) = fixture.indexed();
    let custody = SecretCustody::new([Arc::new(SecretLease::new("split-secret"))]);
    let response = read_projected(
        &workspace,
        &index,
        &fixture.artifacts(),
        &context("split-path-content"),
        &ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("split-"),
            range: ReadRange::Full,
        },
        &ReadOptions::default(),
        &custody,
    )
    .unwrap();

    assert_eq!(response.path, Path::new("split-"));
    assert_eq!(response.content, b"[REDACTED]");
    let mut scanner = custody.redactor().scanner();
    scanner.push(&response.to_canonical_json().unwrap());
    assert!(!scanner.found());
}

#[test]
fn unchanged_projected_partial_read_preserves_source_coordinates_exactly() {
    let fixture = Fixture::new();
    fixture.write("public.txt", b"prefix public suffix\n");
    let (workspace, index) = fixture.indexed();
    let request = ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from("public.txt"),
        range: ReadRange::Bytes { start: 7, end: 13 },
    };
    let baseline = read(
        &workspace,
        &index,
        &fixture.artifacts(),
        &context("plain-read"),
        &request,
        &ReadOptions::default(),
    )
    .unwrap();
    let custody = SecretCustody::new([Arc::new(SecretLease::new("absent-secret"))]);
    let projected = read_projected(
        &workspace,
        &index,
        &fixture.artifacts(),
        &context("plain-read"),
        &request,
        &ReadOptions::default(),
        &custody,
    )
    .unwrap();

    assert_eq!(projected, baseline);
    assert_eq!((projected.byte_start, projected.byte_end), (7, 13));
    assert_eq!(projected.source_offset, None);
    assert_eq!(projected.source_length, None);
    assert_eq!(projected.source_digest, None);
    assert_eq!(projected.projected_offset, None);
    assert_eq!(projected.projected_digest, None);
}

#[test]
fn changed_projected_partial_read_keeps_source_and_projected_coordinates_separate() {
    let fixture = Fixture::new();
    let source = b"prefix workspace-secret suffix\n";
    fixture.write("secret.txt", source);
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let custody = SecretCustody::new([Arc::new(SecretLease::new("workspace-secret"))]);
    let request = ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from("secret.txt"),
        range: ReadRange::Bytes { start: 7, end: 23 },
    };
    let response = read_projected(
        &workspace,
        &index,
        &artifacts,
        &context("partial-projected-read"),
        &request,
        &ReadOptions {
            max_inline_bytes: 4,
            ..ReadOptions::default()
        },
        &custody,
    )
    .unwrap();

    assert_eq!(response.content, b"[RED");
    assert_eq!(response.file_bytes, source.len());
    assert_eq!((response.byte_start, response.byte_end), (0, 10));
    assert_eq!(response.source_offset, Some(7));
    assert_eq!(response.source_length, Some(16));
    assert_eq!(
        response.source_offset.unwrap() + response.source_length.unwrap(),
        23
    );
    assert_eq!(
        response.source_digest.as_deref(),
        Some(
            ArtifactDigest::digest(b"workspace-secret")
                .to_string()
                .as_str()
        )
    );
    assert_eq!(response.source_file_size, Some(source.len()));
    assert_eq!(response.projected_offset, Some(0));
    assert_eq!(response.projected_length, Some(10));
    assert_eq!(
        response.projected_digest.as_deref(),
        Some(ArtifactDigest::digest(b"[REDACTED]").to_string().as_str())
    );
    assert!(response.gap.is_some());
    assert!(response.truncated);
    assert_eq!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &context("partial-projected-read"),
            &request,
            response.artifact.as_ref().unwrap(),
            &ArtifactResolveOptions::default(),
        )
        .unwrap(),
        b"[REDACTED]"
    );
}

#[test]
fn discover_ranking_is_deterministic_diverse_and_explained() {
    let fixture = Fixture::new();
    fixture.write("src/target.rs", "pub fn target() {}\ntarget target\n");
    fixture.write("src/other.rs", "pub fn target_helper() {}\n");
    fixture.write("tests/target_test.rs", "fn checks() { target(); }\n");
    let (workspace, index) = fixture.indexed();
    let options = DiscoverOptions {
        max_results_per_path: 1,
        ..DiscoverOptions::default()
    };

    let first = discover(&workspace, &index, &query("target"), &options, None).unwrap();
    let second = discover(&workspace, &index, &query("target"), &options, None).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.results[0].kind, DiscoverKind::Symbol);
    assert_eq!(first.results[0].symbol.as_deref(), Some("target"));
    assert!(
        first
            .results
            .iter()
            .all(|result| !result.rationale.is_empty())
    );
    let mut paths: Vec<_> = first.results.iter().map(|result| &result.path).collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), first.results.len());
    assert_eq!(first.result_bytes, first.to_canonical_json().unwrap().len());
}

#[test]
fn diversity_is_retained_before_paging_and_duplicate_paths_never_cross_pages() {
    let fixture = Fixture::new();
    fixture.write("needle.rs", "fn needle() { /* needle */ }\n");
    fixture.write("lower-a.rs", "// needle\n");
    fixture.write("lower-b.rs", "// needle\n");
    let (workspace, index) = fixture.indexed();
    let options = DiscoverOptions {
        max_results: 1,
        max_results_per_path: 1,
        ..DiscoverOptions::default()
    };
    let mut cursor = None;
    let mut paths = Vec::new();
    loop {
        let page = discover(
            &workspace,
            &index,
            &query("needle"),
            &options,
            cursor.as_ref(),
        )
        .unwrap();
        assert!(!page.results.is_empty());
        paths.push(page.results[0].path.clone());
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(paths.len(), unique.len());
    assert_eq!(paths.len(), 3);

    assert!(matches!(
        discover(
            &workspace,
            &index,
            &query("needle"),
            &DiscoverOptions {
                max_results_per_path: 10_001,
                ..DiscoverOptions::default()
            },
            None,
        ),
        Err(DiscoverError::InvalidOptions(_))
    ));
}

#[test]
fn focused_line_and_byte_reads_preserve_unicode_crlf_and_missing_newline() {
    let fixture = Fixture::new();
    let source = "α first\r\n東京 second\r\nlast";
    fixture.write("unicode.txt", source);
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let line = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("unicode.txt"),
            range: ReadRange::Lines { start: 2, end: 2 },
        },
        &ReadOptions::default(),
    )
    .unwrap();
    assert_eq!(line.content, "東京 second\r\n".as_bytes());
    assert_eq!(line.newline, NewlineStyle::Crlf);
    assert!(!line.final_newline);
    assert_eq!((line.line_start, line.line_end), (Some(2), Some(2)));

    let start = source.find("東京").unwrap();
    let end = start + "東京".len();
    let bytes = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("unicode.txt"),
            range: ReadRange::Bytes { start, end },
        },
        &ReadOptions::default(),
    )
    .unwrap();
    assert_eq!(bytes.content, "東京".as_bytes());
    assert_eq!((bytes.byte_start, bytes.byte_end), (start, end));
    assert_eq!((bytes.line_start, bytes.line_end), (Some(2), Some(2)));
}

#[test]
fn binary_large_and_full_log_reads_use_revision_and_auth_bound_artifacts() {
    let fixture = Fixture::new();
    fixture.write("binary.bin", b"abc\0def");
    fixture.write("large.txt", vec![b'x'; 4096]);
    fixture.write("run.log", b"short complete log");
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let options = ReadOptions {
        max_inline_bytes: 16,
        ..ReadOptions::default()
    };
    let request = |path: &str| ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from(path),
        range: ReadRange::Full,
    };
    let invoke = |path: &str, principal: &str| {
        read(
            &workspace,
            &index,
            &artifacts,
            &context(principal),
            &request(path),
            &options,
        )
        .unwrap()
    };
    let binary = invoke("binary.bin", "alice");
    assert_eq!(binary.encoding, Encoding::Binary);
    assert!(binary.artifact.is_some());
    let large = invoke("large.txt", "alice");
    assert!(large.artifact.is_some() && large.truncated && large.gap.is_some());
    let log = invoke("run.log", "alice");
    assert!(log.artifact.is_some());

    let alice = binary.artifact.unwrap();
    let bob = invoke("binary.bin", "bob").artifact.unwrap();
    assert_ne!(alice.id, bob.id, "auth context must bind the handle");
    let stored = resolve_artifact(
        &workspace,
        &artifacts,
        &context("alice"),
        &request("binary.bin"),
        &alice,
        &ArtifactResolveOptions::default(),
    )
    .unwrap();
    assert_eq!(stored, b"abc\0def");
    assert!(matches!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &context("bob"),
            &request("binary.bin"),
            &alice,
            &ArtifactResolveOptions::default(),
        ),
        Err(ReadError::ArtifactAuthorization)
    ));
    let mut oversized = context("alice");
    oversized.project = "x".repeat(129);
    assert!(matches!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &oversized,
            &request("binary.bin"),
            &alice,
            &ArtifactResolveOptions::default(),
        ),
        Err(ReadError::InvalidOptions(_))
    ));
}

#[test]
fn durable_v3_workspace_artifact_fixture_resolves_without_v4_semantic_confusion() {
    let fixture = Fixture::new();
    let payload = b"legacy\0workspace-artifact";
    fixture.write("legacy.bin", payload);
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let context = context("legacy-reader");
    let request = ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from("legacy.bin"),
        range: ReadRange::Full,
    };
    let handle = legacy_v3_artifact(&workspace, &artifacts, &context, &request, payload);

    assert_eq!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &context,
            &request,
            &handle,
            &ArtifactResolveOptions::default(),
        )
        .unwrap(),
        payload
    );
    let confused = WorkspaceArtifactHandle {
        id: handle.id.replacen(":v3:", ":v4:", 1),
        path: handle.path.clone(),
        path_digest: handle.path_digest.clone(),
    };
    assert!(matches!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &context,
            &request,
            &confused,
            &ArtifactResolveOptions::default(),
        ),
        Err(ReadError::ArtifactAuthorization)
    ));
}

#[test]
fn artifact_resolver_rejects_tampering_projects_raw_digests_and_unicode_fragments() {
    let fixture = Fixture::new();
    fixture.write("unicode.txt", "αz");
    fixture.write("other.txt", "other");
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let fragment = read(
        &workspace,
        &index,
        &artifacts,
        &context("alice"),
        &ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("unicode.txt"),
            range: ReadRange::Bytes { start: 1, end: 2 },
        },
        &ReadOptions::default(),
    )
    .unwrap();
    assert_eq!(fragment.encoding, Encoding::Binary);
    let handle = fragment.artifact.unwrap();
    let expected = ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from("unicode.txt"),
        range: ReadRange::Bytes { start: 1, end: 2 },
    };
    assert_eq!(
        resolve_artifact(
            &workspace,
            &artifacts,
            &context("alice"),
            &expected,
            &handle,
            &ArtifactResolveOptions::default(),
        )
        .unwrap(),
        &[0xb1]
    );

    let mut other_project = context("alice");
    other_project.project = "other".to_owned();
    let mut tampered = handle.clone();
    let replacement = if &tampered.id[31..32] == "0" {
        "1"
    } else {
        "0"
    };
    tampered.id.replace_range(31..32, replacement);
    assert!(!handle.id.contains("blake3:"));
    for (auth, candidate) in [
        (other_project, handle.clone()),
        (context("alice"), tampered),
    ] {
        assert!(matches!(
            resolve_artifact(
                &workspace,
                &artifacts,
                &auth,
                &expected,
                &candidate,
                &ArtifactResolveOptions::default(),
            ),
            Err(ReadError::ArtifactAuthorization)
        ));
    }
    for substituted in [
        ReadRequest {
            path: PathBuf::from("other.txt"),
            ..expected.clone()
        },
        ReadRequest {
            range: ReadRange::Bytes { start: 0, end: 1 },
            ..expected.clone()
        },
    ] {
        assert!(matches!(
            resolve_artifact(
                &workspace,
                &artifacts,
                &context("alice"),
                &substituted,
                &handle,
                &ArtifactResolveOptions::default(),
            ),
            Err(ReadError::ArtifactAuthorization)
        ));
    }
}

#[test]
fn required_artifacts_over_the_exact_envelope_bound_are_typed() {
    let fixture = Fixture::new();
    fixture.write("binary", b"a\0b");
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    assert!(matches!(
        read(
            &workspace,
            &index,
            &artifacts,
            &context("reader"),
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("binary"),
                range: ReadRange::Full,
            },
            &ReadOptions {
                max_artifact_bytes: 3,
                ..ReadOptions::default()
            },
        ),
        Err(ReadError::ArtifactTooLarge { required, max: 3 }) if required > 3
    ));
}

#[test]
fn staged_artifacts_roll_back_on_stale_timeout_and_serialization_failure() {
    for failure in ["stale", "timeout", "serialization"] {
        let fixture = Fixture::new();
        fixture.write("binary", b"a\0b");
        let (workspace, index) = fixture.indexed();
        let artifacts = fixture.artifacts();
        let mut options = ReadOptions::default();
        if failure == "timeout" {
            options.max_time = Duration::from_millis(100);
        } else if failure == "serialization" {
            options.max_result_bytes = 1;
        }
        let result = read_with_stage_hook(
            &workspace,
            &index,
            &artifacts,
            &context("reader"),
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("binary"),
                range: ReadRange::Full,
            },
            &options,
            || match failure {
                "stale" => fixture.write("binary", b"changed\0"),
                "timeout" => thread::sleep(Duration::from_millis(200)),
                _ => {}
            },
        );
        assert!(match failure {
            "stale" => matches!(
                result,
                Err(ReadError::Revision(RevisionError::StaleRevision { .. }))
            ),
            "timeout" => matches!(result, Err(ReadError::TimeLimit)),
            _ => matches!(result, Err(ReadError::InvalidOptions(_))),
        });
        let pending_files = regular_file_count(&fixture.artifact_path);
        assert_eq!(pending_files, 0, "{failure}");
        drop(artifacts);
        let recovered = fixture.artifacts();
        let report = recovered
            .collect_garbage(&Reachability {
                now_unix_micros: i64::MAX,
                orphan_grace_micros: 0,
                ..Reachability::default()
            })
            .unwrap();
        assert_eq!(
            report.deleted_artifacts.len() + report.deleted_staged_files,
            0
        );
        assert_eq!(regular_file_count(&fixture.artifact_path), 0, "{failure}");
    }
}

#[test]
fn crash_after_pending_promotion_is_unresolvable_and_startup_gcable() {
    let fixture = Fixture::new();
    fixture.write("binary", b"a\0b");
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = read_with_stage_hook(
            &workspace,
            &index,
            &artifacts,
            &context("reader"),
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("binary"),
                range: ReadRange::Full,
            },
            &ReadOptions::default(),
            || panic!("injected promotion crash"),
        );
    }));
    assert!(crashed.is_err());
    assert!(regular_file_count(&fixture.artifact_path) > 0);
    drop(artifacts);
    let recovered = fixture.artifacts();
    let report = recovered
        .collect_garbage(&Reachability {
            now_unix_micros: i64::MAX,
            orphan_grace_micros: 0,
            ..Reachability::default()
        })
        .unwrap();
    assert_eq!(report.deleted_artifacts.len(), 1);
    assert_eq!(regular_file_count(&fixture.artifact_path), 0);
}

#[test]
fn discover_rejects_huge_capacities_before_tiny_candidate_reserve() {
    let fixture = Fixture::new();
    fixture.write("needle", "needle");
    let (workspace, index) = fixture.indexed();
    assert!(matches!(
        discover(
            &workspace,
            &index,
            &query("needle"),
            &DiscoverOptions {
                max_candidate_bytes: 1,
                max_results: usize::MAX,
                max_cursor_offset: usize::MAX,
                ..DiscoverOptions::default()
            },
            None,
        ),
        Err(DiscoverError::InvalidOptions(_))
    ));
}

#[test]
fn discover_rejects_candidate_dynamic_peak_before_cloning() {
    let fixture = Fixture::new();
    fixture.write("a-very-long-needle-path", "needle");
    let (workspace, index) = fixture.indexed();
    let structural = 2 * std::mem::size_of::<kit::workspace::search::discover::DiscoverResult>();
    let response = discover(
        &workspace,
        &index,
        &query("needle"),
        &DiscoverOptions {
            max_candidate_bytes: structural + 4,
            max_results: 1,
            max_results_per_path: 1,
            max_cursor_offset: 1,
            ..DiscoverOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(response.results.is_empty());
    assert!(response.truncated);
}

#[test]
fn edits_during_both_artifact_commit_stages_roll_back_every_handle() {
    for point in [
        ReadPublishPoint::ProvisionalSynced,
        ReadPublishPoint::VerifiedSynced,
    ] {
        let fixture = Fixture::new();
        fixture.write("binary", b"a\0b");
        let (workspace, index) = fixture.indexed();
        let artifacts = fixture.artifacts();
        let result = read_with_publish_hook(
            &workspace,
            &index,
            &artifacts,
            &context("reader"),
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("binary"),
                range: ReadRange::Full,
            },
            &ReadOptions::default(),
            |visited| {
                if visited == point {
                    fixture.write("binary", b"changed\0");
                }
            },
        );
        assert!(matches!(
            result,
            Err(ReadError::Revision(RevisionError::StaleRevision { .. }))
        ));
        assert_eq!(regular_file_count(&fixture.artifact_path), 0, "{point:?}");
    }
}

#[test]
fn deadlines_expiring_at_each_publication_stage_roll_back() {
    for point in [
        ReadPublishPoint::ProvisionalSynced,
        ReadPublishPoint::VerifiedSynced,
        ReadPublishPoint::IssuedSynced,
    ] {
        let fixture = Fixture::new();
        fixture.write("binary", b"a\0b");
        let (workspace, index) = fixture.indexed();
        let artifacts = fixture.artifacts();
        let result = read_with_publish_hook(
            &workspace,
            &index,
            &artifacts,
            &context("reader"),
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("binary"),
                range: ReadRange::Full,
            },
            &ReadOptions {
                max_time: Duration::from_millis(500),
                ..ReadOptions::default()
            },
            |visited| {
                if visited == point {
                    thread::sleep(Duration::from_millis(1_000));
                }
            },
        );
        assert!(matches!(result, Err(ReadError::TimeLimit)), "{point:?}");
        assert_eq!(regular_file_count(&fixture.artifact_path), 0, "{point:?}");
    }
}

#[test]
fn crashed_forever_artifacts_are_resolvable_only_if_issued_and_always_lease_gcable() {
    for crash_point in [
        ReadPublishPoint::ProvisionalSynced,
        ReadPublishPoint::VerifiedSynced,
        ReadPublishPoint::IssuedSynced,
    ] {
        let fixture = Fixture::new();
        fixture.write("binary", b"a\0b");
        let (workspace, index) = fixture.indexed();
        let request = ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("binary"),
            range: ReadRange::Full,
        };
        let issued_store = fixture.root.join("issued-artifacts");
        let issued = ArtifactStore::open(&issued_store).unwrap();
        read(
            &workspace,
            &index,
            &issued,
            &context("reader"),
            &request,
            &ReadOptions::default(),
        )
        .unwrap();
        let digest = only_artifact_digest(&issued_store);
        let crashed_store = fixture.root.join(format!("crashed-{crash_point:?}"));
        let crashed = ArtifactStore::open(&crashed_store).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = read_with_publish_hook(
                &workspace,
                &index,
                &crashed,
                &context("reader"),
                &request,
                &ReadOptions::default(),
                |point| {
                    if point == crash_point {
                        panic!("crash after {crash_point:?}");
                    }
                },
            );
        }));
        assert!(result.is_err());
        assert_eq!(
            crashed.workspace_artifact_is_issued(digest).unwrap(),
            crash_point == ReadPublishPoint::IssuedSynced
        );
        drop(crashed);
        let recovered = ArtifactStore::open(&crashed_store).unwrap();
        let report = recovered
            .collect_garbage(&Reachability {
                now_unix_micros: i64::MAX,
                orphan_grace_micros: 0,
                ..Reachability::default()
            })
            .unwrap();
        assert_eq!(report.deleted_artifacts.len(), 1);
        assert_eq!(regular_file_count(&crashed_store), 0);
    }
}

fn only_artifact_digest(root: &Path) -> ArtifactDigest {
    let objects = root.join("objects");
    let shard = fs::read_dir(objects).unwrap().next().unwrap().unwrap();
    let file = fs::read_dir(shard.path()).unwrap().next().unwrap().unwrap();
    let shard = shard.file_name();
    let file = file.file_name();
    let file = file.to_str().unwrap().strip_suffix(".blob").unwrap();
    ArtifactDigest::parse(&format!("blake3:{}{file}", shard.to_str().unwrap())).unwrap()
}

#[test]
fn oversized_artifact_authorization_is_rejected_before_staging() {
    let fixture = Fixture::new();
    fixture.write("binary", b"a\0b");
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let mut oversized = context("reader");
    oversized.principal = "x".repeat(129);
    assert!(matches!(
        read(
            &workspace,
            &index,
            &artifacts,
            &oversized,
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("binary"),
                range: ReadRange::Full,
            },
            &ReadOptions::default(),
        ),
        Err(ReadError::InvalidOptions(_))
    ));
    assert_eq!(regular_file_count(&fixture.artifact_path), 0);
}

fn regular_file_count(path: &Path) -> usize {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .map(|path| {
            if path.is_dir() {
                regular_file_count(&path)
            } else {
                usize::from(path.is_file())
            }
        })
        .sum()
}

#[test]
fn discover_and_read_bounds_report_omissions_and_exact_serialized_size() {
    let fixture = Fixture::new();
    for index in 0..8 {
        fixture.write(
            &format!("file-{index}.rs"),
            "fn needle() { /* needle */ }\n",
        );
    }
    let (workspace, index) = fixture.indexed();
    let discover_options = DiscoverOptions {
        max_results: 1,
        max_scanned_entries: 2,
        ..DiscoverOptions::default()
    };
    let response = discover(
        &workspace,
        &index,
        &query("needle"),
        &discover_options,
        None,
    )
    .unwrap();
    assert!(response.truncated && !response.omitted_complete && response.cursor.is_none());
    assert!(response.scanned_entries <= 2);
    assert!(response.result_bytes <= discover_options.max_result_bytes);

    let artifacts = fixture.artifacts();
    let bounded = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("file-0.rs"),
            range: ReadRange::Full,
        },
        &ReadOptions {
            max_read_bytes: 10,
            max_inline_bytes: 4,
            ..ReadOptions::default()
        },
    )
    .unwrap();
    assert!(bounded.truncated && bounded.gap.is_some());
    assert_eq!(bounded.content.len(), 4);
    assert_eq!(
        bounded.result_bytes,
        bounded.to_canonical_json().unwrap().len()
    );
}

#[test]
fn focused_read_honors_the_exact_canonical_response_byte_bound() {
    let fixture = Fixture::new();
    fixture.write("exact.txt", vec![b'x'; 1024]);
    let (workspace, index) = fixture.indexed();
    let artifacts = fixture.artifacts();
    let request = ReadRequest {
        expected_revision: index.revision(),
        path: PathBuf::from("exact.txt"),
        range: ReadRange::Full,
    };
    let baseline = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &request,
        &ReadOptions {
            max_inline_bytes: 1024,
            ..ReadOptions::default()
        },
    )
    .unwrap();
    let exact = baseline.result_bytes;
    let at_bound = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &request,
        &ReadOptions {
            max_inline_bytes: 1024,
            max_result_bytes: exact,
            ..ReadOptions::default()
        },
    )
    .unwrap();
    assert_eq!(at_bound.content.len(), 1024);
    assert_eq!(at_bound.result_bytes, exact);
    let below = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &request,
        &ReadOptions {
            max_inline_bytes: 1024,
            max_result_bytes: exact - 1,
            ..ReadOptions::default()
        },
    )
    .unwrap();
    assert!(below.content.len() < 1024);
    assert!(below.result_bytes < exact);
    assert_eq!(below.result_bytes, below.to_canonical_json().unwrap().len());
}

#[test]
fn cursor_and_read_are_typed_stale_after_revision_change_in_100_of_100_attempts() {
    let fixture = Fixture::new();
    fixture.write("a", b"needle");
    fixture.write("b", b"needle");
    let (workspace, index) = fixture.indexed();
    let options = DiscoverOptions {
        max_results: 1,
        max_results_per_path: 1,
        ..DiscoverOptions::default()
    };
    let page = discover(&workspace, &index, &query("needle"), &options, None).unwrap();
    let cursor = page.cursor.unwrap();
    fixture.write("a", b"changed");
    let artifacts = fixture.artifacts();

    for _ in 0..100 {
        assert!(matches!(
            discover(
                &workspace,
                &index,
                &query("needle"),
                &options,
                Some(&cursor)
            ),
            Err(DiscoverError::Revision(RevisionError::StaleRevision { .. }))
        ));
        assert!(matches!(
            read(
                &workspace,
                &index,
                &artifacts,
                &context("reader"),
                &ReadRequest {
                    expected_revision: index.revision(),
                    path: PathBuf::from("b"),
                    range: ReadRange::Full,
                },
                &ReadOptions::default(),
            ),
            Err(ReadError::Revision(RevisionError::StaleRevision { .. }))
        ));
    }
}

#[test]
fn ignored_private_and_noncanonical_paths_cannot_be_discovered_or_read() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", b"ignored/\n");
    fixture.write("ignored/secret", b"needle secret");
    fixture.write(".git/config", b"needle private");
    fixture.write("visible", b"needle public");
    let (workspace, index) = fixture.indexed();
    let found = discover(
        &workspace,
        &index,
        &query("needle"),
        &DiscoverOptions::default(),
        None,
    )
    .unwrap();
    assert!(
        found
            .results
            .iter()
            .all(|result| result.path == Path::new("visible"))
    );
    let artifacts = fixture.artifacts();
    for path in ["ignored/secret", ".git/config"] {
        assert!(matches!(
            read(
                &workspace,
                &index,
                &artifacts,
                &context("reader"),
                &ReadRequest {
                    expected_revision: index.revision(),
                    path: PathBuf::from(path),
                    range: ReadRange::Full,
                },
                &ReadOptions::default(),
            ),
            Err(ReadError::NotIndexed(_))
        ));
    }
    assert!(matches!(
        read(
            &workspace,
            &index,
            &artifacts,
            &context("reader"),
            &ReadRequest {
                expected_revision: index.revision(),
                path: PathBuf::from("../escape"),
                range: ReadRange::Full,
            },
            &ReadOptions::default(),
        ),
        Err(ReadError::UnsafePath(_))
    ));
}

#[test]
fn external_writer_never_produces_a_mixed_focused_read() {
    let fixture = Fixture::new();
    const FILE_BYTES: usize = 256 * 1024;
    fixture.write("large", vec![b'a'; FILE_BYTES]);
    let (workspace, index) = fixture.indexed();
    let workspace = Arc::new(workspace);
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let path = fixture.workspace_path.join("large");
    let writer = thread::spawn(move || {
        let mut byte = b'b';
        while !writer_stop.load(Ordering::Acquire) {
            fs::write(&path, vec![byte; FILE_BYTES]).unwrap();
            byte = if byte == b'b' { b'c' } else { b'b' };
        }
    });
    thread::sleep(Duration::from_millis(5));
    let artifacts = fixture.artifacts();
    let result = read(
        &workspace,
        &index,
        &artifacts,
        &context("reader"),
        &ReadRequest {
            expected_revision: index.revision(),
            path: PathBuf::from("large"),
            range: ReadRange::Bytes {
                start: 0,
                end: 1024,
            },
        },
        &ReadOptions::default(),
    );
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(matches!(
        result,
        Err(ReadError::Revision(
            RevisionError::StaleRevision { .. } | RevisionError::ScanRace { .. }
        ))
    ));
}
