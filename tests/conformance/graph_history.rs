#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use kit::capabilities::native::{NativeCatalog, NativeTool};
use kit::workspace::{
    edit::ir::RootRelativePath,
    graph::history::{
        BlameSource, CoverageArea, CoverageStatus, GitImplementationIdentity, HistoryBound,
        HistoryError, HistoryGraph, HistoryGraphProvider, HistoryOptions, HistoryRequest,
        ObjectFormat, ObjectId,
        test_support::{
            self, GitCommand, GitCommandError, GitCommandLimits, GitCommandOutput,
            HistoryCommandRunner, StagingAllocation, TrustedGitRunner,
        },
    },
    graph::structure::{
        CoverageStatus as GraphCoverageStatus, EdgeKind, GraphOptions, HistoryEnrichmentLimits,
        StructureGraphProvider,
    },
    index::meta::{IndexOptions, MetadataIndex},
    map::{
        ExpansionPurpose, ExpansionRequest, MapBudget, MapError, MapLimits, RelationshipKind,
        RepositoryMapRequest, build_repository_map_with_history,
        build_repository_map_with_structure,
    },
    revision::{ManagedWorkspace, RevisionOptions},
};

struct Fixture {
    root: PathBuf,
    repository: PathBuf,
    commits: Vec<String>,
}

impl Fixture {
    fn empty(name: &str) -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-history-{name}-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let repository = root.join("repo");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-q", "--object-format=sha1"]);
        git(&repository, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        Self {
            root,
            repository,
            commits: Vec::new(),
        }
    }

    fn pinned() -> Self {
        let mut fixture = Self::empty("pinned");
        let states = [
            [("alpha.txt", "alpha one\n"), ("beta.txt", "beta one\n")].as_slice(),
            [
                ("alpha.txt", "alpha one\nalpha two\n"),
                ("beta.txt", "beta two\n"),
            ]
            .as_slice(),
            [
                ("beta.txt", "beta two\n"),
                ("gamma.txt", "alpha one\nalpha two\n"),
            ]
            .as_slice(),
            [
                ("beta.txt", "beta two\n"),
                ("delta.txt", "delta four\n"),
                ("gamma.txt", "alpha one\ngamma four\n"),
            ]
            .as_slice(),
            [
                ("beta.txt", "beta two\n"),
                ("delta.txt", "delta four\n"),
                ("gamma.txt", "alpha one\ngamma four\n"),
            ]
            .as_slice(),
            [
                ("beta.txt", "beta six\n"),
                ("delta.txt", "delta six\n"),
                ("gamma.txt", "alpha one\ngamma four\n"),
            ]
            .as_slice(),
            [
                ("beta.txt", "beta seven\n"),
                ("delta.txt", "delta six\n"),
                ("gamma.txt", "alpha one\ngamma four\ngamma seven\n"),
            ]
            .as_slice(),
        ];
        let mut parent: Option<String> = None;
        for (index, state) in states.iter().enumerate() {
            let tree = fixture.tree(state);
            let mut arguments = vec!["commit-tree".to_owned(), tree];
            if let Some(parent) = &parent {
                arguments.extend(["-p".to_owned(), parent.clone()]);
            }
            let timestamp = format!("1700000{index:03} +0000");
            let commit = git_input_env(
                &fixture.repository,
                &arguments.iter().map(String::as_str).collect::<Vec<_>>(),
                format!("commit {}\n", index + 1).as_bytes(),
                &timestamp,
            );
            fixture.commits.push(commit.clone());
            parent = Some(commit);
        }
        git(
            &fixture.repository,
            &[
                "update-ref",
                "refs/heads/main",
                fixture.commits.last().unwrap(),
            ],
        );
        for (path, contents) in states.last().unwrap().iter() {
            fs::write(fixture.repository.join(path), contents).unwrap();
        }
        fixture
    }

    fn tree(&self, files: &[(&str, &str)]) -> String {
        let mut records = BTreeMap::new();
        for (path, contents) in files {
            let oid = git_input(
                &self.repository,
                &["hash-object", "-w", "--stdin"],
                contents.as_bytes(),
            );
            records.insert(*path, oid);
        }
        let mut input = Vec::new();
        for (path, oid) in records {
            input.extend_from_slice(format!("100644 blob {oid}\t{path}\0").as_bytes());
        }
        git_input(&self.repository, &["mktree", "-z"], &input)
    }

    fn open(&self) -> ManagedWorkspace {
        ManagedWorkspace::open_with_options(
            &self.repository,
            RevisionOptions {
                max_entries: 1_000,
                max_name_bytes: 1024 * 1024,
                max_bytes: 16 * 1024 * 1024,
                max_memory_bytes: 32 * 1024 * 1024,
                max_depth: 64,
                max_scan_time: Duration::from_secs(5),
                max_scan_attempts: 2,
                watcher_interval: Duration::from_millis(5),
                reconciliation_interval: Duration::from_secs(60),
                metadata_path: Some(self.root.join("revision.state")),
            },
        )
        .unwrap()
    }

    fn runner(&self, workspace: &ManagedWorkspace) -> TrustedGitRunner {
        TrustedGitRunner::new(workspace).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(repository: &Path, arguments: &[&str]) -> String {
    git_input(repository, arguments, &[])
}

fn git_input(repository: &Path, arguments: &[&str], input: &[u8]) -> String {
    git_command(repository, arguments, input, None)
}

fn git_input_env(repository: &Path, arguments: &[&str], input: &[u8], date: &str) -> String {
    git_command(repository, arguments, input, Some(date))
}

fn git_command(repository: &Path, arguments: &[&str], input: &[u8], date: Option<&str>) -> String {
    let mut command = Command::new("/usr/bin/git");
    command
        .current_dir(repository)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/bin:/bin")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(date) = date {
        command
            .env("GIT_AUTHOR_NAME", "Kit Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_NAME", "Kit Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_DATE", date);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Git {:?}: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn path(value: &str) -> RootRelativePath {
    RootRelativePath::parse(value, 4_096).unwrap()
}

fn build_index(workspace: &ManagedWorkspace) -> MetadataIndex {
    let revision = workspace.current_revision().unwrap().id();
    MetadataIndex::build(workspace, revision, &IndexOptions::default()).unwrap()
}

fn request() -> HistoryRequest {
    HistoryRequest::all(vec![path("gamma.txt")])
}

fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn object_ids_accept_only_canonical_sha1_and_sha256() {
    assert_eq!(
        ObjectId::parse(&"a".repeat(40), ObjectFormat::Sha1)
            .unwrap()
            .as_str(),
        "a".repeat(40)
    );
    assert_eq!(
        ObjectId::parse(&"b".repeat(64), ObjectFormat::Sha256)
            .unwrap()
            .as_str(),
        "b".repeat(64)
    );
    for value in ["A".repeat(40), "g".repeat(40), "0".repeat(64)] {
        assert!(ObjectId::parse(&value, ObjectFormat::Sha1).is_err());
    }
}

#[test]
fn provider_api_has_release_shape() {
    let _: for<'a> fn(
        &'a mut HistoryGraphProvider,
        &ManagedWorkspace,
        &MetadataIndex,
        &HistoryRequest,
        &HistoryOptions,
    ) -> Result<&'a HistoryGraph, HistoryError> = HistoryGraphProvider::refresh;
    let _: for<'a> fn(
        &'a HistoryGraphProvider,
        &ManagedWorkspace,
    ) -> Result<Option<&'a HistoryGraph>, HistoryError> = HistoryGraphProvider::validated_graph;
}

#[test]
fn pinned_seven_commit_fixture_has_exact_rename_cochange_and_blame() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let graph = provider
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap();

    assert_eq!(graph.commits().len(), 7);
    assert_eq!(graph.head().as_str(), fixture.commits[6]);
    assert_eq!(graph.renames().len(), 1);
    assert_eq!(graph.renames()[0].commit().as_str(), fixture.commits[2]);
    assert_eq!(graph.renames()[0].from().as_str(), "alpha.txt");
    assert_eq!(graph.renames()[0].to().as_str(), "gamma.txt");
    assert_eq!(
        graph.renames()[0].current_path().unwrap().as_str(),
        "gamma.txt"
    );
    assert_eq!(graph.renames()[0].confidence_millis(), 1_000);

    let pairs = graph
        .changed_with()
        .iter()
        .map(|fact| ((fact.left().as_str(), fact.right().as_str()), fact.count()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(pairs[&("beta.txt", "gamma.txt")], 2);
    assert_eq!(pairs[&("beta.txt", "delta.txt")], 1);
    assert_eq!(pairs[&("delta.txt", "gamma.txt")], 1);
    assert!(graph.changed_with().iter().all(|fact| {
        fact.left() < fact.right()
            && fact.extraction_confidence_millis() == 1_000
            && fact.provenance().count() == fact.count()
            && fact.provenance().shared_count() == fact.count()
            && fact.provenance().revision() == index.revision()
    }));

    let blame = graph
        .blame_hunks()
        .iter()
        .filter(|hunk| hunk.source() == BlameSource::Git)
        .collect::<Vec<_>>();
    assert_eq!(blame.len(), 3);
    assert_eq!(blame[0].source_path().as_str(), "alpha.txt");
    assert_eq!(
        blame[0].source_commit().unwrap().as_str(),
        fixture.commits[0]
    );
    assert_eq!(blame[1].source_path().as_str(), "gamma.txt");
    assert_eq!(
        blame[1].source_commit().unwrap().as_str(),
        fixture.commits[3]
    );
    assert_eq!(
        blame[2].source_commit().unwrap().as_str(),
        fixture.commits[6]
    );
    assert!(graph.coverage().iter().any(|item| {
        item.area() == CoverageArea::Commits && item.status() == CoverageStatus::Complete
    }));

    let golden = hex(graph.content_digest());
    assert_eq!(
        golden,
        "76ce8b5e0717c1364d08fd7834c62c2d9744eae55c0a29f5959f6d8583256d81"
    );
    for _ in 0..9 {
        let mut cold = HistoryGraphProvider::new();
        let candidate = cold
            .refresh(&workspace, &index, &request(), &HistoryOptions::default())
            .unwrap();
        assert_eq!(candidate.content_digest(), graph.content_digest());
    }
    let independent = Fixture::pinned();
    let independent_workspace = independent.open();
    let independent_index = build_index(&independent_workspace);
    let mut independent_provider = HistoryGraphProvider::new();
    let independent_graph = independent_provider
        .refresh(
            &independent_workspace,
            &independent_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert_eq!(independent.commits, fixture.commits);
    assert_eq!(independent_graph.head(), graph.head());
    assert_eq!(independent_graph.request_digest(), graph.request_digest());
    assert_ne!(independent_graph.snapshot_digest(), graph.snapshot_digest());
}

#[test]
fn pinned_history_enriches_one_canonical_graph_with_exact_typed_provenance() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut history_provider = HistoryGraphProvider::new();
    let history = history_provider
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let history_fence = history_provider
        .validated_fence(&workspace)
        .unwrap()
        .unwrap();
    let fence_metrics = history_fence.metrics();
    assert_eq!(fence_metrics.policy_scans(), 1);
    assert_eq!(fence_metrics.validations(), 1);
    assert_eq!(fence_metrics.commands(), 4);
    assert!(fence_metrics.fence_scans() >= 7);
    assert!(fence_metrics.streamed_executable_bytes() > 0);
    assert!(fence_metrics.streamed_executable_chunks() > 0);
    assert!(fence_metrics.peak_memory_bytes() >= fence_metrics.logical_memory_bytes());
    assert!(fence_metrics.work() >= fence_metrics.streamed_executable_bytes());
    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    assert!(structure.coverage().iter().any(|record| {
        record.relation() == EdgeKind::ChangedWith
            && record.status() == GraphCoverageStatus::Unavailable
    }));
    let graph = history
        .enrich_structure(&structure, HistoryEnrichmentLimits::default())
        .unwrap();
    assert_eq!(graph.history().unwrap().head(), history.head());
    assert_eq!(graph.history().unwrap().head_tree(), history.head_tree());
    let edge = graph
        .edges()
        .iter()
        .find(|edge| {
            edge.kind() == EdgeKind::ChangedWith
                && graph
                    .nodes()
                    .iter()
                    .find(|node| node.id() == edge.source())
                    .and_then(|node| node.path())
                    .is_some_and(|path| path.as_str() == "beta.txt")
                && graph
                    .nodes()
                    .iter()
                    .find(|node| node.id() == edge.target())
                    .and_then(|node| node.path())
                    .is_some_and(|path| path.as_str() == "gamma.txt")
        })
        .unwrap();
    assert!(edge.provenance().semantic().is_none());
    let provenance = edge.provenance().history().unwrap();
    assert_eq!(provenance.head(), history.head());
    assert_eq!(provenance.head_tree(), history.head_tree());
    assert_eq!(provenance.left_path().as_str(), "beta.txt");
    assert_eq!(provenance.right_path().as_str(), "gamma.txt");
    assert_eq!(provenance.count(), 2);
    assert_eq!(provenance.shared_count(), 2);
    let fact = history
        .changed_with()
        .iter()
        .find(|fact| fact.left().as_str() == "beta.txt" && fact.right().as_str() == "gamma.txt")
        .unwrap();
    assert_eq!(provenance.strength_millis(), fact.strength_millis());
    assert_eq!(provenance.support_commits().len(), 2);
    assert_eq!(provenance.revision(), index.revision());
    assert_eq!(provenance.extraction_confidence_millis(), 1_000);
    assert_ne!(provenance.policy_digest(), [0; 32]);
    assert_eq!(provenance.extractor_digest(), history.extractor_digest());
    assert_eq!(
        provenance.evidence_digest(),
        edge.provenance().evidence_digest()
    );

    let request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![path("delta.txt")],
            purpose: ExpansionPurpose::Neighborhood,
            relationships: vec![RelationshipKind::ChangedWith],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    let expected = build_repository_map_with_history(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        None,
        &graph,
        &history_fence,
    )
    .unwrap()
    .to_canonical_json()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&expected).unwrap();
    let measured_items = value["item_count"].as_u64().unwrap() as usize;
    let measured_tokens = value["estimated_tokens"].as_u64().unwrap() as usize;
    let measured_bytes = value["result_bytes"].as_u64().unwrap() as usize;
    let paths = value["graph_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["path"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        paths,
        std::collections::BTreeSet::from(["beta.txt", "delta.txt", "gamma.txt"])
    );
    assert_eq!(value["history"]["head"], history.head().as_str());
    assert_eq!(value["history"]["cochange_completeness"], "complete");
    assert_eq!(
        value["history"]["git_implementation_digest"],
        hex(history.git_implementation().digest())
    );
    let edges = value["graph_edges"].as_array().unwrap();
    assert_eq!(edges.len(), 3);
    assert!(edges.iter().all(|edge| {
        edge["relationship"] == "changed_with"
            && edge["provenance"]["source"] == "git_history"
            && edge["provenance"]["semantic"].is_null()
            && edge["provenance"]["history"]["left_path"].as_str()
                < edge["provenance"]["history"]["right_path"].as_str()
    }));
    let beta_gamma = edges
        .iter()
        .find(|edge| {
            edge["provenance"]["history"]["left_path"] == "beta.txt"
                && edge["provenance"]["history"]["right_path"] == "gamma.txt"
        })
        .unwrap();
    assert_eq!(
        beta_gamma["provenance"]["history"]["strength_millis"],
        fact.strength_millis()
    );
    assert_eq!(
        beta_gamma["provenance"]["confidence_millis"],
        fact.extraction_confidence_millis()
    );
    let openapi: serde_json::Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap();
    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/components/schemas/RepositoryMapResponse",
        "components": openapi["components"].clone(),
    });
    let validator = jsonschema::draft202012::options().build(&schema).unwrap();
    assert!(
        validator.is_valid(&value),
        "runtime repository map diverged from OpenAPI: {:?}",
        validator.iter_errors(&value).collect::<Vec<_>>()
    );
    let mut invalid = value.clone();
    invalid["history"]["head"] = serde_json::json!("a".repeat(64));
    assert!(!validator.is_valid(&invalid));
    let mut invalid = value.clone();
    invalid["history"]["commits_omitted"] = serde_json::json!(1);
    assert!(!validator.is_valid(&invalid));
    let mut invalid = value.clone();
    invalid["graph_edges"][0]["provenance"]["semantic"] = serde_json::Value::Null;
    assert!(!validator.is_valid(&invalid));
    let mut invalid = value.clone();
    invalid["graph_edges"][0]["provenance"]["history"]["policy"] = serde_json::json!("approximate");
    assert!(!validator.is_valid(&invalid));
    let native_output = serde_json::json!({
        "version": 1,
        "data": {"mode": "map", "map": value, "semanticEvidenceAvailable": false},
        "artifacts": [],
        "truncated": false
    });
    let discover = NativeCatalog::all()
        .iter()
        .find(|descriptor| descriptor.tool() == NativeTool::Discover)
        .unwrap();
    let catalog_output = discover.spec().output_schema.as_ref().unwrap();
    let catalog_validator = jsonschema::draft202012::options()
        .build(catalog_output)
        .unwrap();
    assert!(
        catalog_validator.is_valid(&native_output),
        "runtime native output diverged from catalog schema: {:?}",
        catalog_validator
            .iter_errors(&native_output)
            .collect::<Vec<_>>()
    );
    let result_id = format!("tool_call_0{}", "a".repeat(25));
    let repository_result = serde_json::json!({
        "schema_version": 1,
        "id": result_id,
        "operation": "repo.discover",
        "status": "completed",
        "replayed": false,
        "output": native_output,
        "error": null,
        "cost": null,
        "artifacts": null
    });
    let result_schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/components/schemas/RepositoryResult",
        "components": openapi["components"].clone(),
    });
    let result_validator = jsonschema::draft202012::options()
        .build(&result_schema)
        .unwrap();
    assert!(
        result_validator.is_valid(&repository_result),
        "runtime repository result diverged from OpenAPI: {:?}",
        result_validator
            .iter_errors(&repository_result)
            .collect::<Vec<_>>()
    );
    for (budget, bound) in [
        (
            MapBudget {
                max_items: measured_items - 1,
                ..MapBudget::default()
            },
            kit::workspace::map::MapBound::Items,
        ),
        (
            MapBudget {
                max_estimated_tokens: measured_tokens - 1,
                ..MapBudget::default()
            },
            kit::workspace::map::MapBound::EstimatedTokens,
        ),
        (
            MapBudget {
                max_hops: 0,
                ..MapBudget::default()
            },
            kit::workspace::map::MapBound::Hops,
        ),
        (
            MapBudget {
                max_degree: 1,
                ..MapBudget::default()
            },
            kit::workspace::map::MapBound::Degree,
        ),
        (
            MapBudget {
                max_result_bytes: measured_bytes - 1,
                ..MapBudget::default()
            },
            kit::workspace::map::MapBound::ResultBytes,
        ),
    ] {
        let mut bounded = request.clone();
        bounded.budget = budget;
        assert!(matches!(
            build_repository_map_with_history(
                &workspace,
                &index,
                &bounded,
                &[],
                MapLimits::default(),
                None,
                &graph,
                &history_fence,
            ),
            Err(MapError::BoundExceeded(actual)) if actual == bound
        ));
    }

    for _ in 0..10 {
        let mut candidate_provider = HistoryGraphProvider::new();
        let history = candidate_provider
            .refresh(
                &workspace,
                &index,
                &HistoryRequest::default(),
                &HistoryOptions::default(),
            )
            .unwrap()
            .clone();
        let candidate_fence = candidate_provider
            .validated_fence(&workspace)
            .unwrap()
            .unwrap();
        let structure = StructureGraphProvider::new()
            .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
            .unwrap()
            .clone();
        let candidate = history
            .enrich_structure(&structure, HistoryEnrichmentLimits::default())
            .unwrap();
        assert_eq!(candidate.content_digest(), graph.content_digest());
        assert_eq!(candidate.snapshot_digest(), graph.snapshot_digest());
        let map = build_repository_map_with_history(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None,
            &candidate,
            &candidate_fence,
        )
        .unwrap();
        assert_eq!(map.to_canonical_json().unwrap(), expected);
    }
}

#[test]
fn enrichment_is_precharged_bounded_and_idempotent_from_the_structural_base() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let history = HistoryGraphProvider::new()
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let enriched = structure
        .with_history(&history, HistoryEnrichmentLimits::default())
        .unwrap();
    assert_eq!(
        enriched.structural_content_digest(),
        structure.content_digest()
    );
    assert_eq!(
        enriched.structural_snapshot_digest(),
        structure.snapshot_digest()
    );
    assert_eq!(
        enriched
            .with_history(&history, HistoryEnrichmentLimits::default())
            .unwrap(),
        enriched
    );
    let peak = enriched.enrichment_peak_bytes();
    structure
        .with_history(
            &history,
            HistoryEnrichmentLimits {
                max_memory_bytes: peak,
                ..HistoryEnrichmentLimits::default()
            },
        )
        .unwrap();
    assert!(matches!(
        structure.with_history(
            &history,
            HistoryEnrichmentLimits {
                max_memory_bytes: peak - 1,
                ..HistoryEnrichmentLimits::default()
            },
        ),
        Err(kit::workspace::graph::structure::GraphError::BoundExceeded(
            kit::workspace::graph::structure::GraphBound::StagingBytes
        ))
    ));
    assert!(matches!(
        structure.with_history(
            &history,
            HistoryEnrichmentLimits {
                max_work: 1,
                ..HistoryEnrichmentLimits::default()
            },
        ),
        Err(kit::workspace::graph::structure::GraphError::BoundExceeded(
            kit::workspace::graph::structure::GraphBound::Work
        ))
    ));
    assert!(matches!(
        structure.with_history(
            &history,
            HistoryEnrichmentLimits {
                max_time: Duration::from_nanos(1),
                ..HistoryEnrichmentLimits::default()
            },
        ),
        Err(kit::workspace::graph::structure::GraphError::BoundExceeded(
            kit::workspace::graph::structure::GraphBound::Time
        ))
    ));
}

#[test]
fn blame_only_has_no_repo_wide_cochange_and_map_returns_digest_only_hunks() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let request = HistoryRequest::blame_only(vec![path("gamma.txt")]);
    let mut provider = HistoryGraphProvider::new();
    let history = provider
        .refresh(&workspace, &index, &request, &HistoryOptions::default())
        .unwrap()
        .clone();
    assert!(!request.include_changed_with());
    assert!(history.changed_with().is_empty());
    assert!(history.coverage().iter().any(|record| {
        record.area() == CoverageArea::CoChange && record.status() == CoverageStatus::Unavailable
    }));
    let fence = provider.validated_fence(&workspace).unwrap().unwrap();
    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let graph = history
        .enrich_structure(&structure, HistoryEnrichmentLimits::default())
        .unwrap();
    let map_request = RepositoryMapRequest {
        blame_paths: vec![path("gamma.txt")],
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map_with_structure(
            &workspace,
            &index,
            &map_request,
            &[],
            MapLimits::default(),
            None,
            Some(&graph),
        ),
        Err(MapError::HistoryEvidenceUnavailable)
    ));
    let value: serde_json::Value = serde_json::from_slice(
        &build_repository_map_with_history(
            &workspace,
            &index,
            &map_request,
            &[],
            MapLimits::default(),
            None,
            &graph,
            &fence,
        )
        .unwrap()
        .to_canonical_json()
        .unwrap(),
    )
    .unwrap();
    let blame = value["blame"].as_array().unwrap();
    assert!(!blame.is_empty());
    assert!(blame.iter().all(|hunk| {
        hunk["path"] == "gamma.txt"
            && hunk["line_digest"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
            && hunk["evidence_digest"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
            && hunk.get("text").is_none()
    }));
    assert_eq!(value["history"]["cochange_completeness"], "unavailable");
    assert_eq!(value["history"]["blame_completeness"], "complete");
    let openapi: serde_json::Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap();
    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/components/schemas/RepositoryMapResponse",
        "components": openapi["components"].clone(),
    });
    let validator = jsonschema::draft202012::options().build(&schema).unwrap();
    assert!(validator.is_valid(&value));
    let mut invalid = value.clone();
    invalid["blame"][0]["source_commit"] = serde_json::Value::Null;
    assert!(!validator.is_valid(&invalid));
    let mut invalid = value.clone();
    invalid.as_object_mut().unwrap().remove("history");
    assert!(!validator.is_valid(&invalid));
}

#[test]
fn blame_is_page_one_only_and_history_scope_is_fence_bound() {
    let mut fixture = Fixture::empty("blame-pagination");
    let tree = fixture.tree(&[
        ("alpha.rs", "fn alpha() {}\n"),
        ("beta.rs", "fn beta() {}\n"),
        ("gamma.rs", "fn gamma() {}\n"),
    ]);
    let commit = git_input_env(
        &fixture.repository,
        &["commit-tree", &tree],
        b"root\n",
        "1700080000 +0000",
    );
    fixture.commits.push(commit.clone());
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &commit],
    );
    for name in ["alpha", "beta", "gamma"] {
        fs::write(
            fixture.repository.join(format!("{name}.rs")),
            format!("fn {name}() {{}}\n"),
        )
        .unwrap();
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let history_request = HistoryRequest::blame_only(vec![path("alpha.rs")]);
    let mut provider = HistoryGraphProvider::new();
    let history = provider
        .refresh(
            &workspace,
            &index,
            &history_request,
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let fence = provider.validated_fence(&workspace).unwrap().unwrap();
    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let graph = history
        .enrich_structure(&structure, HistoryEnrichmentLimits::default())
        .unwrap();
    let request = RepositoryMapRequest {
        budget: MapBudget {
            max_items: 2,
            ..MapBudget::default()
        },
        blame_paths: history_request.blame_paths().to_vec(),
        ..RepositoryMapRequest::default()
    };
    let first = build_repository_map_with_history(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        None,
        &graph,
        &fence,
    )
    .unwrap();
    let first_value: serde_json::Value =
        serde_json::from_slice(&first.to_canonical_json().unwrap()).unwrap();
    assert!(!first_value["blame"].as_array().unwrap().is_empty());
    let second = build_repository_map_with_history(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        first.cursor(),
        &graph,
        &fence,
    )
    .unwrap();
    let second_value: serde_json::Value =
        serde_json::from_slice(&second.to_canonical_json().unwrap()).unwrap();
    assert!(second_value.get("blame").is_none());
    assert_eq!(
        first_value["history"]["request_digest"],
        second_value["history"]["request_digest"]
    );

    let mismatched = RepositoryMapRequest {
        blame_paths: vec![path("beta.rs")],
        ..request
    };
    assert!(matches!(
        build_repository_map_with_history(
            &workspace,
            &index,
            &mismatched,
            &[],
            MapLimits::default(),
            None,
            &graph,
            &fence,
        ),
        Err(MapError::GraphEvidenceStale)
    ));
}

#[test]
fn default_scope_omits_untracked_and_dirty_ranges_keep_committed_evidence_separate() {
    let fixture = Fixture::pinned();
    fs::write(fixture.repository.join("untracked.txt"), "not at HEAD\n").unwrap();
    fs::write(
        fixture.repository.join("gamma.txt"),
        "alpha one\ngamma four\ngamma seven\nworktree line\n",
    )
    .unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let graph = provider
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert!(graph.coverage().iter().any(|record| {
        record.area() == CoverageArea::CoChange
            && record.omitted_count() == 1
            && record.status() == CoverageStatus::ObservedPartial
    }));
    let fact = graph
        .changed_with()
        .iter()
        .find(|fact| fact.right().as_str() == "gamma.txt")
        .unwrap();
    assert_eq!(
        fact.provenance().right_range().end_byte(),
        fs::read(fixture.repository.join("gamma.txt"))
            .unwrap()
            .len()
    );
    assert_ne!(
        fact.provenance().right_range(),
        fact.provenance().right_committed_range()
    );
    assert!(!fact.provenance().right_committed_blob().as_str().is_empty());
}

#[test]
fn exact_rename_counts_once_after_raw_record_bounding() {
    let mut fixture = Fixture::empty("rename-count");
    let empty = fixture.tree(&[]);
    let root = git_input_env(
        &fixture.repository,
        &["commit-tree", &empty],
        b"empty\n",
        "1700080000 +0000",
    );
    let added_tree = fixture.tree(&[("old.txt", "same\n")]);
    let added = git_input_env(
        &fixture.repository,
        &["commit-tree", &added_tree, "-p", &root],
        b"add\n",
        "1700080001 +0000",
    );
    let renamed_tree = fixture.tree(&[("new.txt", "same\n")]);
    let renamed = git_input_env(
        &fixture.repository,
        &["commit-tree", &renamed_tree, "-p", &added],
        b"rename\n",
        "1700080002 +0000",
    );
    fixture.commits = vec![root, added, renamed.clone()];
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &renamed],
    );
    fs::write(fixture.repository.join("new.txt"), "same\n").unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let graph = provider
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions {
                max_changes: 2,
                max_raw_changes: 3,
                ..HistoryOptions::default()
            },
        )
        .unwrap();
    assert_eq!(graph.changes().len(), 2);
    assert_eq!(graph.renames().len(), 1);
    assert_eq!(
        graph
            .changes()
            .iter()
            .filter(|change| change.kind() == kit::workspace::graph::history::ChangeKind::Renamed)
            .count(),
        1
    );
}

#[test]
fn changed_with_requires_history_and_rejects_directional_purposes() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let mut request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![path("delta.txt")],
            relationships: vec![RelationshipKind::ChangedWith],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map_with_structure(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None,
            Some(&structure),
        ),
        Err(MapError::HistoryEvidenceUnavailable)
    ));
    for purpose in [ExpansionPurpose::Dependencies, ExpansionPurpose::Dependents] {
        request.expansion.purpose = purpose;
        assert!(matches!(
            build_repository_map_with_structure(
                &workspace,
                &index,
                &request,
                &[],
                MapLimits::default(),
                None,
                Some(&structure),
            ),
            Err(MapError::InvalidRequest(
                "changed_with is valid only for neighborhood traversal"
            ))
        ));
    }
}

#[test]
fn same_tree_history_change_invalidates_map_cursor() {
    let mut fixture = Fixture::empty("map-cursor");
    let first_tree = fixture.tree(&[
        ("alpha.rs", "fn alpha() {}\n"),
        ("delta.rs", "fn delta() {}\n"),
        ("gamma.rs", "fn gamma() {}\n"),
    ]);
    let first = git_input_env(
        &fixture.repository,
        &["commit-tree", &first_tree],
        b"first\n",
        "1700070000 +0000",
    );
    let second_tree = fixture.tree(&[
        ("alpha.rs", "fn alpha() { let _ = 1; }\n"),
        ("delta.rs", "fn delta() { let _ = 1; }\n"),
        ("gamma.rs", "fn gamma() { let _ = 1; }\n"),
    ]);
    let second = git_input_env(
        &fixture.repository,
        &["commit-tree", &second_tree, "-p", &first],
        b"second\n",
        "1700070001 +0000",
    );
    fixture.commits = vec![first, second.clone()];
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &second],
    );
    for (name, source) in [
        ("alpha.rs", "fn alpha() { let _ = 1; }\n"),
        ("delta.rs", "fn delta() { let _ = 1; }\n"),
        ("gamma.rs", "fn gamma() { let _ = 1; }\n"),
    ] {
        fs::write(fixture.repository.join(name), source).unwrap();
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let history_request = HistoryRequest::new(
        vec![path("alpha.rs"), path("delta.rs"), path("gamma.rs")],
        Vec::new(),
        true,
    );
    let mut history_provider = HistoryGraphProvider::new();
    let history = history_provider
        .refresh(
            &workspace,
            &index,
            &history_request,
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let history_fence = history_provider
        .validated_fence(&workspace)
        .unwrap()
        .unwrap();
    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let graph = history
        .enrich_structure(&structure, HistoryEnrichmentLimits::default())
        .unwrap();
    let neighborhood = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![path("delta.rs")],
            relationships: vec![RelationshipKind::ChangedWith],
            ..ExpansionRequest::default()
        },
        history_paths: history_request.scope().to_vec(),
        ..RepositoryMapRequest::default()
    };
    let neighborhood: serde_json::Value = serde_json::from_slice(
        &build_repository_map_with_history(
            &workspace,
            &index,
            &neighborhood,
            &[],
            MapLimits::default(),
            None,
            &graph,
            &history_fence,
        )
        .unwrap()
        .to_canonical_json()
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        neighborhood["graph_nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|node| node["path"].as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["alpha.rs", "delta.rs", "gamma.rs"])
    );
    let request = RepositoryMapRequest {
        budget: MapBudget {
            max_items: 1,
            ..MapBudget::default()
        },
        expansion: ExpansionRequest {
            relationships: vec![RelationshipKind::ChangedWith],
            ..ExpansionRequest::default()
        },
        history_paths: history_request.scope().to_vec(),
        ..RepositoryMapRequest::default()
    };
    let first_page = build_repository_map_with_history(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        None,
        &graph,
        &history_fence,
    )
    .unwrap();
    let cursor = first_page.cursor().unwrap().clone();

    let third = git_input_env(
        &fixture.repository,
        &["commit-tree", &second_tree, "-p", &second],
        b"same tree\n",
        "1700070002 +0000",
    );
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &third],
    );
    let stale = history_provider.validated_graph(&workspace);
    assert!(
        matches!(
            stale,
            Err(HistoryError::StaleRepositoryFence | HistoryError::Revision(_))
        ),
        "{stale:?}"
    );
    let next_index = build_index(&workspace);
    let next_history = history_provider
        .refresh(
            &workspace,
            &next_index,
            &history_request,
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let next_fence = history_provider
        .validated_fence(&workspace)
        .unwrap()
        .unwrap();
    let next_structure = StructureGraphProvider::new()
        .refresh(&workspace, &next_index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let next_graph = next_history
        .enrich_structure(&next_structure, HistoryEnrichmentLimits::default())
        .unwrap();
    assert_ne!(next_graph.snapshot_digest(), graph.snapshot_digest());
    assert!(matches!(
        build_repository_map_with_history(
            &workspace,
            &next_index,
            &request,
            &[],
            MapLimits::default(),
            Some(&cursor),
            &next_graph,
            &next_fence,
        ),
        Err(MapError::CursorMismatch)
    ));
}

#[test]
fn limits_are_exact_and_failures_preserve_graph_cache_and_metrics() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut measured = HistoryGraphProvider::new();
    measured
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap();
    let work = measured.metrics().consumed_work();
    let commands = measured.metrics().commands();
    let output = measured.metrics().output_bytes();
    let staging = measured.metrics().peak_staging_bytes();

    for (exact, below, bound) in [
        (
            HistoryOptions {
                max_commands: commands,
                ..HistoryOptions::default()
            },
            HistoryOptions {
                max_commands: commands - 1,
                ..HistoryOptions::default()
            },
            HistoryBound::Commands,
        ),
        (
            HistoryOptions {
                max_work: work,
                ..HistoryOptions::default()
            },
            HistoryOptions {
                max_work: work - 1,
                ..HistoryOptions::default()
            },
            HistoryBound::Work,
        ),
        (
            HistoryOptions {
                max_output_bytes: output,
                ..HistoryOptions::default()
            },
            HistoryOptions {
                max_output_bytes: output - 1,
                ..HistoryOptions::default()
            },
            HistoryBound::OutputBytes,
        ),
        (
            HistoryOptions {
                max_staging_bytes: staging,
                ..HistoryOptions::default()
            },
            HistoryOptions {
                max_staging_bytes: staging - 1,
                ..HistoryOptions::default()
            },
            HistoryBound::StagingBytes,
        ),
    ] {
        let mut provider = HistoryGraphProvider::new();
        provider
            .refresh(&workspace, &index, &request(), &exact)
            .unwrap_or_else(|error| panic!("exact {bound:?} failed: {error:?}"));
        let mut provider = HistoryGraphProvider::new();
        assert!(matches!(
            provider.refresh(
                &workspace,
                &index,
                &request(),
                &below,
            ),
            Err(HistoryError::BoundExceeded(actual)) if actual == bound
        ));
    }

    let graph = measured.graph().unwrap().clone();
    let metrics = measured.metrics().clone();
    let cache = measured.cache_usage();
    assert!(matches!(
        measured.refresh(
            &workspace,
            &index,
            &request(),
            &HistoryOptions {
                max_commits: 6,
                ..HistoryOptions::default()
            },
        ),
        Err(HistoryError::BoundExceeded(HistoryBound::Commits))
    ));
    assert_eq!(measured.graph(), Some(&graph));
    assert_eq!(measured.metrics(), &metrics);
    assert_eq!(measured.cache_usage(), cache);
}

#[test]
fn blame_many_source_blobs_has_an_exact_staging_low_bound() {
    let mut fixture = Fixture::empty("blame-staging");
    let mut parent = None;
    let mut contents = String::new();
    for index in 0..12 {
        contents.push_str(&format!("line {index}\n"));
        let tree = fixture.tree(&[("history.txt", &contents)]);
        let mut arguments = vec!["commit-tree", tree.as_str()];
        if let Some(parent) = parent.as_deref() {
            arguments.extend(["-p", parent]);
        }
        let commit = git_input_env(
            &fixture.repository,
            &arguments,
            format!("line {index}\n").as_bytes(),
            &format!("1700050{index:03} +0000"),
        );
        fixture.commits.push(commit.clone());
        parent = Some(commit);
    }
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", parent.as_deref().unwrap()],
    );
    fs::write(fixture.repository.join("history.txt"), contents).unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let request = HistoryRequest::all(vec![path("history.txt")]);
    let mut measured = HistoryGraphProvider::new();
    let graph = measured
        .refresh(&workspace, &index, &request, &HistoryOptions::default())
        .unwrap();
    assert_eq!(graph.blame_hunks().len(), 12);
    assert_eq!(
        graph
            .blame_hunks()
            .iter()
            .filter_map(|hunk| hunk.source_blob())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        12
    );
    let staging = measured.metrics().peak_staging_bytes();

    let mut exact = HistoryGraphProvider::new();
    exact
        .refresh(
            &workspace,
            &index,
            &request,
            &HistoryOptions {
                max_staging_bytes: staging,
                ..HistoryOptions::default()
            },
        )
        .unwrap();
    let mut below = HistoryGraphProvider::new();
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &request,
            &HistoryOptions {
                max_staging_bytes: staging - 1,
                ..HistoryOptions::default()
            },
        ),
        Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
    ));
}

#[test]
fn temporary_staging_preflights_fail_before_allocation() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let options = HistoryOptions {
        max_cache_entries: 8,
        ..HistoryOptions::default()
    };
    let mut measured_runner = InstrumentedRunner::new(fixture.runner(&workspace));
    test_support::refresh_with_runner(
        &mut HistoryGraphProvider::new(),
        &workspace,
        &index,
        &request(),
        &options,
        &mut measured_runner,
    )
    .unwrap();

    for allocation in [
        StagingAllocation::CacheEvictionCandidates,
        StagingAllocation::PruneSets,
        StagingAllocation::RenameKeys,
    ] {
        let required_peak = measured_runner
            .staging_allocations
            .iter()
            .find_map(|(observed, peak)| (*observed == allocation).then_some(*peak))
            .unwrap_or_else(|| panic!("{allocation:?} was not exercised"));

        let mut exact_runner = InstrumentedRunner::new(fixture.runner(&workspace));
        let _ = test_support::refresh_with_runner(
            &mut HistoryGraphProvider::new(),
            &workspace,
            &index,
            &request(),
            &HistoryOptions {
                max_staging_bytes: required_peak,
                ..options
            },
            &mut exact_runner,
        );
        assert!(
            exact_runner
                .staging_allocations
                .iter()
                .any(|(observed, _)| *observed == allocation),
            "exact {allocation:?} preflight did not reach allocation"
        );

        let mut below_runner = InstrumentedRunner::new(fixture.runner(&workspace));
        assert!(matches!(
            test_support::refresh_with_runner(
                &mut HistoryGraphProvider::new(),
                &workspace,
                &index,
                &request(),
                &HistoryOptions {
                    max_staging_bytes: required_peak - 1,
                    ..options
                },
                &mut below_runner,
            ),
            Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
        ));
        assert!(
            below_runner
                .staging_allocations
                .iter()
                .all(|(observed, _)| *observed != allocation),
            "low {allocation:?} bound allocated temporary storage"
        );
    }
}

#[test]
fn cochange_preflight_reserves_exact_upper_bound_before_map_population() {
    let mut fixture = Fixture::empty("cochange-preflight");
    let before = (0..64)
        .map(|index| (format!("path-{index:03}.txt"), format!("before {index}\n")))
        .collect::<Vec<_>>();
    let after = (0..64)
        .map(|index| (format!("path-{index:03}.txt"), format!("after {index}\n")))
        .collect::<Vec<_>>();
    let before_refs = before
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect::<Vec<_>>();
    let after_refs = after
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect::<Vec<_>>();
    let base_tree = fixture.tree(&before_refs);
    let base = git_input_env(
        &fixture.repository,
        &["commit-tree", &base_tree],
        b"base\n",
        "1700060000 +0000",
    );
    let next_tree = fixture.tree(&after_refs);
    let next = git_input_env(
        &fixture.repository,
        &["commit-tree", &next_tree, "-p", &base],
        b"all changed\n",
        "1700060001 +0000",
    );
    fixture.commits = vec![base, next.clone()];
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &next],
    );
    for (path, contents) in &after {
        fs::write(fixture.repository.join(path), contents).unwrap();
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut measured = HistoryGraphProvider::new();
    measured
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        )
        .unwrap();
    let preflight = test_support::cochange_preflight_staging_bytes(measured.metrics());

    let mut below_runner = InstrumentedRunner::new(fixture.runner(&workspace));
    assert!(matches!(
        test_support::refresh_with_runner(
            &mut HistoryGraphProvider::new(),
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions {
                max_staging_bytes: preflight - 1,
                ..HistoryOptions::default()
            },
            &mut below_runner,
        ),
        Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
    ));
    assert_eq!(below_runner.map_entries, 0);

    let mut exact_runner = InstrumentedRunner::new(fixture.runner(&workspace));
    let mut exact_provider = HistoryGraphProvider::new();
    let exact_result = test_support::refresh_with_runner(
        &mut exact_provider,
        &workspace,
        &index,
        &HistoryRequest::default(),
        &HistoryOptions {
            max_staging_bytes: preflight,
            ..HistoryOptions::default()
        },
        &mut exact_runner,
    );
    assert_eq!(
        exact_runner.map_entries,
        64 + (64 * 63 / 2),
        "{exact_result:?}"
    );

    let structure = StructureGraphProvider::new()
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &[])
        .unwrap()
        .clone();
    let enriched = measured
        .graph()
        .unwrap()
        .enrich_structure(&structure, HistoryEnrichmentLimits::default())
        .unwrap();
    assert!(
        enriched
            .edges()
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::ChangedWith)
            .count()
            >= 64 * 63 / 2
    );
    let peak = enriched.enrichment_peak_bytes();
    structure
        .with_history(
            measured.graph().unwrap(),
            HistoryEnrichmentLimits {
                max_memory_bytes: peak,
                ..HistoryEnrichmentLimits::default()
            },
        )
        .unwrap();
    assert!(
        structure
            .with_history(
                measured.graph().unwrap(),
                HistoryEnrichmentLimits {
                    max_memory_bytes: peak - 1,
                    ..HistoryEnrichmentLimits::default()
                },
            )
            .is_err()
    );
}

#[test]
fn worktree_overlay_changes_content_and_snapshot_identity() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let first_index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let first = provider
        .refresh(
            &workspace,
            &first_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    fs::write(fixture.repository.join("gamma.txt"), "dirty overlay\n").unwrap();
    let second_index = build_index(&workspace);
    let second = provider
        .refresh(
            &workspace,
            &second_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert_ne!(second.content_digest(), first.content_digest());
    assert_ne!(second.snapshot_digest(), first.snapshot_digest());
    assert!(
        second
            .blame_hunks()
            .iter()
            .any(|hunk| hunk.source() == BlameSource::Worktree)
    );
}

#[test]
fn empty_same_tree_head_is_re_resolved_and_hostile_hooks_and_helpers_are_not_run() {
    let fixture = Fixture::pinned();
    let sentinel = fixture.root.join("sentinel");
    let helper = fixture.root.join("hostile-helper");
    fs::write(
        &helper,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &fixture.repository,
        &["config", "core.fsmonitor", helper.to_str().unwrap()],
    );
    git(
        &fixture.repository,
        &["config", "core.hooksPath", helper.to_str().unwrap()],
    );
    git(
        &fixture.repository,
        &["config", "diff.hostile.textconv", helper.to_str().unwrap()],
    );
    fs::write(
        fixture.repository.join(".git/info/attributes"),
        "gamma.txt diff=hostile\n",
    )
    .unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let first = provider
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap()
        .clone();
    assert!(!sentinel.exists());

    let tree = git(&fixture.repository, &["rev-parse", "HEAD^{tree}"]);
    let parent = first.head().as_str().to_owned();
    let next = git_input_env(
        &fixture.repository,
        &["commit-tree", &tree, "-p", &parent],
        b"empty same tree\n",
        "1700001000 +0000",
    );
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &next],
    );
    let next_index = build_index(&workspace);
    let second = provider
        .refresh(
            &workspace,
            &next_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert_eq!(second.commits().len(), 8);
    assert_eq!(second.head().as_str(), next);
    assert_ne!(second.content_digest(), first.content_digest());
    assert_ne!(second.snapshot_digest(), first.snapshot_digest());
    assert!(!sentinel.exists());
}

#[test]
fn validated_graph_rejects_same_tree_new_head_until_refresh() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    provider
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap();

    let tree = git(&fixture.repository, &["rev-parse", "HEAD^{tree}"]);
    let parent = fixture.commits.last().unwrap();
    let next = git_input_env(
        &fixture.repository,
        &["commit-tree", &tree, "-p", parent],
        b"same tree fence\n",
        "1700002000 +0000",
    );
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &next],
    );

    assert!(provider.validated_graph(&workspace).is_err());
    let next_index = build_index(&workspace);
    provider
        .refresh(
            &workspace,
            &next_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert_eq!(
        provider
            .validated_graph(&workspace)
            .unwrap()
            .unwrap()
            .head()
            .as_str(),
        next
    );
}

#[test]
fn runner_repository_root_is_bound_to_workspace() {
    let left = Fixture::pinned();
    let right = Fixture::pinned();
    let workspace = left.open();
    let right_workspace = right.open();
    let index = build_index(&workspace);
    assert!(matches!(
        test_support::refresh_with_runner(
            &mut HistoryGraphProvider::new(),
            &workspace,
            &index,
            &request(),
            &HistoryOptions::default(),
            &mut right.runner(&right_workspace),
        ),
        Err(HistoryError::RepositoryRootMismatch { .. })
    ));
}

#[test]
fn trusted_runner_rejects_path_replacement_instead_of_reading_repository_b() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let mut runner = fixture.runner(&workspace);
    let original = fixture.root.join("original-repo");
    fs::rename(&fixture.repository, &original).unwrap();
    fs::create_dir(&fixture.repository).unwrap();
    git(&fixture.repository, &["init", "-q", "--object-format=sha1"]);
    fs::write(fixture.repository.join("other.txt"), "repository b\n").unwrap();
    git(&fixture.repository, &["add", "other.txt"]);
    git(
        &fixture.repository,
        &[
            "-c",
            "user.name=Repository B",
            "-c",
            "user.email=b@example.invalid",
            "commit",
            "-q",
            "-m",
            "repository b",
        ],
    );
    let repository_b = git(&fixture.repository, &["rev-parse", "HEAD"]);

    let result = runner.run(
        &GitCommand::Head,
        GitCommandLimits {
            timeout: Duration::from_secs(2),
            stdout_bytes: 4_096,
            stderr_bytes: 4_096,
        },
    );
    assert!(result.is_err());
    assert!(!matches!(
        result,
        Ok(ref output) if String::from_utf8_lossy(output.stdout()).trim() == repository_b
    ));
}

#[test]
fn trusted_reads_allow_sanitized_config_and_reject_config_and_pack_policy_bypasses() {
    let assert_head = |fixture: &Fixture, workspace: &ManagedWorkspace, accepted: bool| {
        let result = fixture.runner(workspace).run(
            &GitCommand::Head,
            GitCommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: 4_096,
                stderr_bytes: 4_096,
            },
        );
        assert_eq!(result.is_ok(), accepted, "{result:?}");
    };

    let sanitized = Fixture::pinned();
    let workspace = sanitized.open();
    fs::remove_file(sanitized.repository.join(".git/config")).unwrap();
    assert_head(&sanitized, &workspace, true);

    for value in [
        "true",
        "yes",
        "on",
        "1",
        "\"true\"",
        "true # enabled",
        "tr\\\nue",
        "false\\q",
    ] {
        let fixture = Fixture::pinned();
        let workspace = fixture.open();
        fs::write(
            fixture.repository.join(".git/config"),
            format!("[remote \"origin\"]\n\tpromisor = {value}\n"),
        )
        .unwrap();
        assert_head(&fixture, &workspace, false);
    }

    for value in [
        "false",
        "no",
        "off",
        "0",
        "\"false\"",
        "false # disabled",
        "fal\\\nse",
    ] {
        let fixture = Fixture::pinned();
        let workspace = fixture.open();
        fs::write(
            fixture.repository.join(".git/config"),
            format!("[remote \"origin\"]\n\tpromisor = {value}\n"),
        )
        .unwrap();
        assert_head(&fixture, &workspace, true);
    }

    for config in [
        "[remote \"origin\"]\n\tpromisor\n",
        "[include]\n\tpath = ../hostile\n",
        "[extensions]\n\tpartialClone = origin\n",
    ] {
        let fixture = Fixture::pinned();
        let workspace = fixture.open();
        fs::write(fixture.repository.join(".git/config"), config).unwrap();
        assert_head(&fixture, &workspace, false);
    }

    let worktree_config = Fixture::pinned();
    let workspace = worktree_config.open();
    fs::write(
        worktree_config.repository.join(".git/config.worktree"),
        "[remote \"origin\"]\n\tpromisor = true\n",
    )
    .unwrap();
    assert_head(&worktree_config, &workspace, false);

    let oversized = Fixture::pinned();
    let workspace = oversized.open();
    fs::write(
        oversized.repository.join(".git/config"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .unwrap();
    assert_head(&oversized, &workspace, false);

    let pack_symlink = Fixture::pinned();
    let workspace = pack_symlink.open();
    std::os::unix::fs::symlink(
        "/dev/null",
        pack_symlink
            .repository
            .join(".git/objects/pack/hostile.idx"),
    )
    .unwrap();
    assert_head(&pack_symlink, &workspace, false);

    let objects_symlink = Fixture::pinned();
    let workspace = objects_symlink.open();
    let objects = objects_symlink.repository.join(".git/objects");
    fs::rename(
        &objects,
        objects_symlink.repository.join(".git/objects-real"),
    )
    .unwrap();
    std::os::unix::fs::symlink("objects-real", &objects).unwrap();
    assert_head(&objects_symlink, &workspace, false);

    let fanout_symlink = Fixture::pinned();
    let workspace = fanout_symlink.open();
    let head = git(&fanout_symlink.repository, &["rev-parse", "HEAD"]);
    let objects = fanout_symlink.repository.join(".git/objects");
    let fanout = objects.join(&head[..2]);
    let real_fanout = objects.join(format!("{}-real", &head[..2]));
    fs::rename(&fanout, &real_fanout).unwrap();
    std::os::unix::fs::symlink(real_fanout.file_name().unwrap(), &fanout).unwrap();
    assert_head(&fanout_symlink, &workspace, false);
}

#[test]
fn trusted_reads_nofollow_and_open_commit_graph_and_midx_metadata() {
    let assert_rejected = |fixture: &Fixture, workspace: &ManagedWorkspace| {
        assert!(
            fixture
                .runner(workspace)
                .run(
                    &GitCommand::Head,
                    GitCommandLimits {
                        timeout: Duration::from_secs(2),
                        stdout_bytes: 4_096,
                        stderr_bytes: 4_096,
                    },
                )
                .is_err()
        );
    };

    let commit_graph_link = Fixture::pinned();
    let workspace = commit_graph_link.open();
    std::os::unix::fs::symlink(
        "/dev/null",
        commit_graph_link
            .repository
            .join(".git/objects/info/commit-graph"),
    )
    .unwrap();
    assert_rejected(&commit_graph_link, &workspace);

    let midx_fifo = Fixture::pinned();
    let workspace = midx_fifo.open();
    let path = midx_fifo
        .repository
        .join(".git/objects/pack/multi-pack-index");
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: path is a valid NUL-terminated pathname in the owned fixture.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert_rejected(&midx_fifo, &workspace);

    use std::os::unix::fs::PermissionsExt;
    let unreadable = Fixture::pinned();
    let workspace = unreadable.open();
    let path = unreadable.repository.join(".git/objects/info/commit-graph");
    fs::write(&path, b"not a graph").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    assert_rejected(&unreadable, &workspace);
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn refresh_policy_validation_is_constant_scan_and_rejects_between_command_mutation() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    provider
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap();
    assert!(provider.metrics().commands() > 10);
    assert_eq!(test_support::validation_scans(provider.metrics()), 2);

    for (name, relative, bytes) in [
        (
            "config",
            ".git/config",
            b"[core]\n\trepositoryformatversion = 0\n".as_slice(),
        ),
        (
            "objects",
            ".git/objects/info/mutated",
            b"mutation".as_slice(),
        ),
    ] {
        let mutated_fixture = Fixture::pinned();
        let mutated_workspace = mutated_fixture.open();
        let mutated_index = build_index(&mutated_workspace);
        let mut runner = InstrumentedRunner::new(mutated_fixture.runner(&mutated_workspace));
        let path = mutated_fixture.repository.join(relative);
        runner.mutation = Some((path, bytes.to_vec()));
        let mut candidate = HistoryGraphProvider::new();
        let result = test_support::refresh_with_runner(
            &mut candidate,
            &mutated_workspace,
            &mutated_index,
            &request(),
            &HistoryOptions::default(),
            &mut runner,
        );
        assert!(result.is_err(), "{name} mutation was accepted");
        assert!(runner.mutated);
        assert!(candidate.graph().is_none());
    }
}

#[test]
fn trusted_repository_validation_obeys_tiny_caller_deadline() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let result = fixture.runner(&workspace).run(
        &GitCommand::Head,
        GitCommandLimits {
            timeout: Duration::from_nanos(1),
            stdout_bytes: 4_096,
            stderr_bytes: 4_096,
        },
    );
    assert!(
        matches!(result, Err(GitCommandError::TimedOut)),
        "{result:?}"
    );
}

struct ContractRunner {
    root: PathBuf,
    delay: Duration,
    bytes: usize,
}

struct InstrumentedRunner {
    inner: TrustedGitRunner,
    mutation: Option<(PathBuf, Vec<u8>)>,
    mutated: bool,
    map_entries: usize,
    staging_allocations: Vec<(StagingAllocation, usize)>,
}

struct IdentityRunner {
    inner: TrustedGitRunner,
    identity: GitImplementationIdentity,
}

impl InstrumentedRunner {
    fn new(inner: TrustedGitRunner) -> Self {
        Self {
            inner,
            mutation: None,
            mutated: false,
            map_entries: 0,
            staging_allocations: Vec::new(),
        }
    }
}

impl HistoryCommandRunner for InstrumentedRunner {
    fn canonical_repository_root(&self) -> &Path {
        self.inner.repository()
    }

    fn begin_refresh(&mut self, deadline: std::time::Instant) -> Result<(), GitCommandError> {
        self.inner.begin_refresh(deadline)
    }

    fn finish_refresh(&mut self, deadline: std::time::Instant) -> Result<(), GitCommandError> {
        self.inner.finish_refresh(deadline)
    }

    fn abort_refresh(&mut self) {
        self.inner.abort_refresh();
    }

    fn cochange_map_entry(&mut self) {
        self.map_entries += 1;
    }

    fn staging_allocation(&mut self, allocation: StagingAllocation, required_peak: usize) {
        self.staging_allocations.push((allocation, required_peak));
    }

    fn implementation_identity(&self) -> GitImplementationIdentity {
        self.inner.implementation_identity()
    }

    fn policy_metrics(&self) -> test_support::RepositoryPolicyMetrics {
        self.inner.policy_metrics()
    }

    fn run(
        &mut self,
        command: &GitCommand,
        limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError> {
        let output = self.inner.run(command, limits);
        if output.is_ok()
            && let Some((path, bytes)) = self.mutation.take()
        {
            fs::write(path, bytes).unwrap();
            self.mutated = true;
        }
        output
    }
}

impl HistoryCommandRunner for IdentityRunner {
    fn canonical_repository_root(&self) -> &Path {
        self.inner.repository()
    }

    fn begin_refresh(&mut self, deadline: std::time::Instant) -> Result<(), GitCommandError> {
        self.inner.begin_refresh(deadline)
    }

    fn finish_refresh(&mut self, deadline: std::time::Instant) -> Result<(), GitCommandError> {
        self.inner.finish_refresh(deadline)
    }

    fn abort_refresh(&mut self) {
        self.inner.abort_refresh();
    }

    fn implementation_identity(&self) -> GitImplementationIdentity {
        self.identity.clone()
    }

    fn policy_metrics(&self) -> test_support::RepositoryPolicyMetrics {
        self.inner.policy_metrics()
    }

    fn run(
        &mut self,
        command: &GitCommand,
        limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError> {
        self.inner.run(command, limits)
    }
}

impl HistoryCommandRunner for ContractRunner {
    fn canonical_repository_root(&self) -> &Path {
        &self.root
    }

    fn run(
        &mut self,
        _command: &GitCommand,
        _limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError> {
        std::thread::sleep(self.delay);
        Ok(GitCommandOutput::new(vec![b'x'; self.bytes]))
    }
}

#[test]
fn provider_revalidates_mock_output_and_time_contracts() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut oversized = ContractRunner {
        root: fixture.repository.clone(),
        delay: Duration::ZERO,
        bytes: 9,
    };
    assert!(matches!(
        test_support::refresh_with_runner(
            &mut HistoryGraphProvider::new(),
            &workspace,
            &index,
            &request(),
            &HistoryOptions {
                max_command_output_bytes: 8,
                ..HistoryOptions::default()
            },
            &mut oversized,
        ),
        Err(HistoryError::BoundExceeded(
            HistoryBound::CommandOutputBytes
        ))
    ));

    let mut slow = ContractRunner {
        root: fixture.repository.clone(),
        delay: Duration::from_millis(60),
        bytes: 4,
    };
    assert!(matches!(
        test_support::refresh_with_runner(
            &mut HistoryGraphProvider::new(),
            &workspace,
            &index,
            &request(),
            &HistoryOptions {
                max_time: Duration::from_millis(50),
                ..HistoryOptions::default()
            },
            &mut slow,
        ),
        Err(HistoryError::BoundExceeded(HistoryBound::Time))
    ));
}

#[test]
fn merge_deletion_does_not_conflict_with_surviving_path_identity() {
    let mut fixture = Fixture::empty("merge-survival");
    let base_tree = fixture.tree(&[("old.txt", "same\n")]);
    let base = git_input_env(
        &fixture.repository,
        &["commit-tree", &base_tree],
        b"base\n",
        "1700010000 +0000",
    );
    let survive_tree = fixture.tree(&[("new.txt", "same\n")]);
    let survive = git_input_env(
        &fixture.repository,
        &["commit-tree", &survive_tree, "-p", &base],
        b"survive\n",
        "1700010001 +0000",
    );
    let empty_tree = fixture.tree(&[]);
    let deleted = git_input_env(
        &fixture.repository,
        &["commit-tree", &empty_tree, "-p", &base],
        b"delete\n",
        "1700010002 +0000",
    );
    let merge = git_input_env(
        &fixture.repository,
        &["commit-tree", &survive_tree, "-p", &survive, "-p", &deleted],
        b"merge\n",
        "1700010003 +0000",
    );
    fixture.commits = vec![base, survive.clone(), deleted, merge.clone()];
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &merge],
    );
    fs::write(fixture.repository.join("new.txt"), "same\n").unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let graph = HistoryGraphProvider::new()
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let rename = graph
        .renames()
        .iter()
        .find(|rename| rename.commit().as_str() == survive)
        .unwrap();
    assert_eq!(rename.current_path().unwrap().as_str(), "new.txt");
    assert!(!graph.coverage().iter().any(|item| {
        item.area() == CoverageArea::Renames && item.status() == CoverageStatus::Unavailable
    }));
}

#[test]
fn rename_coverage_is_globally_partial_for_exact_only_renames() {
    for (name, before, after, expected) in [
        (
            "unrelated-rename",
            "alpha alpha alpha\n",
            "completely different material\n",
            CoverageStatus::ObservedPartial,
        ),
        (
            "modified-rename",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            "one\ntwo\nthree\nfour\nfive changed\nsix\nseven\neight\nnine\nten\n",
            CoverageStatus::ObservedPartial,
        ),
    ] {
        let mut fixture = Fixture::empty(name);
        let base_tree = fixture.tree(&[("old.txt", before)]);
        let base = git_input_env(
            &fixture.repository,
            &["commit-tree", &base_tree],
            b"base\n",
            "1700030000 +0000",
        );
        let next_tree = fixture.tree(&[("new.txt", after)]);
        let next = git_input_env(
            &fixture.repository,
            &["commit-tree", &next_tree, "-p", &base],
            b"next\n",
            "1700030001 +0000",
        );
        fixture.commits = vec![base, next.clone()];
        git(
            &fixture.repository,
            &["update-ref", "refs/heads/main", &next],
        );
        fs::write(fixture.repository.join("new.txt"), after).unwrap();
        let workspace = fixture.open();
        let index = build_index(&workspace);
        let graph = HistoryGraphProvider::new()
            .refresh(
                &workspace,
                &index,
                &HistoryRequest::default(),
                &HistoryOptions::default(),
            )
            .unwrap()
            .clone();
        assert!(
            graph
                .coverage()
                .iter()
                .any(|item| { item.area() == CoverageArea::Renames && item.status() == expected })
        );
    }
}

#[test]
fn exact_rename_output_is_independent_of_git_rename_configuration() {
    let left = Fixture::pinned();
    let right = Fixture::pinned();
    git(&left.repository, &["config", "diff.renames", "false"]);
    git(&right.repository, &["config", "diff.renames", "copies"]);
    git(&right.repository, &["config", "diff.renameLimit", "1"]);
    let left_workspace = left.open();
    let right_workspace = right.open();
    let left_index = build_index(&left_workspace);
    let right_index = build_index(&right_workspace);
    let mut left_provider = HistoryGraphProvider::new();
    let left_graph = left_provider
        .refresh(
            &left_workspace,
            &left_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap();
    let mut right_provider = HistoryGraphProvider::new();
    let right_graph = right_provider
        .refresh(
            &right_workspace,
            &right_index,
            &request(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert_eq!(left_graph.content_digest(), right_graph.content_digest());
    assert_eq!(left_graph.renames(), right_graph.renames());
}

#[test]
fn observed_git_implementation_identity_is_bound_into_history_content() {
    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let extract = |marker: u8| {
        let mut provider = HistoryGraphProvider::new();
        let mut runner = IdentityRunner {
            inner: fixture.runner(&workspace),
            identity: GitImplementationIdentity::new(
                "/usr/bin/git".to_owned(),
                [marker; 32],
                [0x56; 32],
            ),
        };
        let graph = test_support::refresh_with_runner(
            &mut provider,
            &workspace,
            &index,
            &request(),
            &HistoryOptions::default(),
            &mut runner,
        )
        .unwrap();
        (
            graph.extractor_digest(),
            graph.content_digest(),
            graph.snapshot_digest(),
        )
    };
    let left = extract(1);
    let right = extract(2);
    assert_ne!(left.0, right.0);
    assert_ne!(left.1, right.1);
    assert_ne!(left.2, right.2);
    assert_ne!(
        GitImplementationIdentity::new("/usr/bin/git".to_owned(), [1; 32], [2; 32]).digest(),
        GitImplementationIdentity::new("/opt/bin/git".to_owned(), [1; 32], [2; 32]).digest(),
    );
}

#[test]
fn materialization_reserves_deleted_paths_that_are_longer_than_every_head_path() {
    let fixture = Fixture::empty("long-deleted-path");
    let deleted = format!("{}.txt", "historical-path-".repeat(14));
    let base_tree = fixture.tree(&[(deleted.as_str(), "gone\n"), ("head.txt", "kept\n")]);
    let base = git_input_env(
        &fixture.repository,
        &["commit-tree", &base_tree],
        b"base\n",
        "1700090000 +0000",
    );
    let head_tree = fixture.tree(&[("head.txt", "kept\n")]);
    let head = git_input_env(
        &fixture.repository,
        &["commit-tree", &head_tree, "-p", &base],
        b"delete long path\n",
        "1700090001 +0000",
    );
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &head],
    );
    fs::write(fixture.repository.join("head.txt"), "kept\n").unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let graph = provider
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert!(
        graph
            .changes()
            .iter()
            .any(|change| change.path().as_str() == deleted)
    );
}

#[test]
fn scope_without_cochange_edges_changes_request_and_content_digests() {
    let mut fixture = Fixture::empty("scope-digest");
    let tree = fixture.tree(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let commit = git_input_env(
        &fixture.repository,
        &["commit-tree", &tree],
        b"root\n",
        "1700020000 +0000",
    );
    fixture.commits.push(commit.clone());
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", &commit],
    );
    fs::write(fixture.repository.join("a.txt"), "a\n").unwrap();
    fs::write(fixture.repository.join("b.txt"), "b\n").unwrap();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut left = HistoryGraphProvider::new();
    let left = left
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::new(vec![path("a.txt")], Vec::new(), true),
            &HistoryOptions::default(),
        )
        .unwrap()
        .clone();
    let mut right = HistoryGraphProvider::new();
    let right = right
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::new(vec![path("b.txt")], Vec::new(), true),
            &HistoryOptions::default(),
        )
        .unwrap();
    assert!(left.changed_with().is_empty() && right.changed_with().is_empty());
    assert_ne!(left.scope_digest(), right.scope_digest());
    assert_ne!(left.request_digest(), right.request_digest());
    assert_ne!(left.content_digest(), right.content_digest());
    assert_ne!(left.snapshot_digest(), right.snapshot_digest());
}

#[test]
fn large_same_tree_history_is_bounded_by_edges_not_commits_times_paths() {
    let mut fixture = Fixture::empty("same-tree-large");
    let files = (0..128)
        .map(|index| (format!("path-{index:03}.txt"), format!("value {index}\n")))
        .collect::<Vec<_>>();
    let borrowed = files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect::<Vec<_>>();
    let tree = fixture.tree(&borrowed);
    let mut parent = None;
    for index in 0..10 {
        let mut arguments = vec!["commit-tree", &tree];
        if let Some(parent) = parent.as_deref() {
            arguments.extend(["-p", parent]);
        }
        let commit = git_input_env(
            &fixture.repository,
            &arguments,
            format!("same tree {index}\n").as_bytes(),
            &format!("1700040{index:03} +0000"),
        );
        fixture.commits.push(commit.clone());
        parent = Some(commit);
    }
    git(
        &fixture.repository,
        &["update-ref", "refs/heads/main", parent.as_deref().unwrap()],
    );
    for (path, contents) in files {
        fs::write(fixture.repository.join(path), contents).unwrap();
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let graph = provider
        .refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions {
                max_commands: 20,
                max_paths: 128,
                max_changes: 128,
                ..HistoryOptions::default()
            },
        )
        .unwrap();
    assert_eq!(graph.commits().len(), 10);
    assert_eq!(graph.changes().len(), 128);
    assert!(provider.metrics().commands() <= 20);
}

struct FaultRunner {
    inner: TrustedGitRunner,
    malformed: bool,
    commits: usize,
}

impl HistoryCommandRunner for FaultRunner {
    fn canonical_repository_root(&self) -> &Path {
        self.inner.repository()
    }

    fn run(
        &mut self,
        command: &GitCommand,
        limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError> {
        if matches!(command, GitCommand::Commit(_)) {
            self.commits += 1;
            if self.malformed {
                return Ok(GitCommandOutput::new(b"tree invalid\n\n".to_vec()));
            }
            if self.commits > 1 {
                return Err(GitCommandError::Failed("missing object".into()));
            }
        }
        self.inner.run(command, limits)
    }
}

#[test]
fn unborn_malformed_missing_and_promisor_repositories_are_typed() {
    let unborn = Fixture::empty("unborn");
    let workspace = unborn.open();
    let index = build_index(&workspace);
    assert!(matches!(
        HistoryGraphProvider::new().refresh(
            &workspace,
            &index,
            &HistoryRequest::default(),
            &HistoryOptions::default(),
        ),
        Err(HistoryError::Unavailable(_))
    ));

    let fixture = Fixture::pinned();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    for malformed in [true, false] {
        let mut runner = FaultRunner {
            inner: fixture.runner(&workspace),
            malformed,
            commits: 0,
        };
        let error = test_support::refresh_with_runner(
            &mut HistoryGraphProvider::new(),
            &workspace,
            &index,
            &request(),
            &HistoryOptions::default(),
            &mut runner,
        )
        .unwrap_err();
        if malformed {
            assert!(matches!(error, HistoryError::Malformed(_)));
        } else {
            assert!(matches!(error, HistoryError::MissingObject(_)));
        }
    }

    git(
        &fixture.repository,
        &["config", "remote.origin.promisor", "true"],
    );
    let index = build_index(&workspace);
    assert!(matches!(
        HistoryGraphProvider::new().refresh(
            &workspace,
            &index,
            &request(),
            &HistoryOptions::default(),
        ),
        Err(HistoryError::Unavailable(_))
    ));
}

struct UnshallowDuringPruneRunner {
    inner: TrustedGitRunner,
    repository: PathBuf,
    unshallowed: bool,
}

impl HistoryCommandRunner for UnshallowDuringPruneRunner {
    fn canonical_repository_root(&self) -> &Path {
        self.inner.repository()
    }

    fn before_prune_cache(&mut self) -> Result<(), HistoryError> {
        git(&self.repository, &["fetch", "-q", "--unshallow", "origin"]);
        self.unshallowed = true;
        Ok(())
    }

    fn run(
        &mut self,
        command: &GitCommand,
        limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError> {
        self.inner.run(command, limits)
    }
}

#[test]
fn final_fence_rejects_unshallow_during_prune() {
    let source = Fixture::pinned();
    let shallow = Fixture::empty("shallow-prune-fence");
    fs::remove_dir_all(&shallow.repository).unwrap();
    git(
        &shallow.root,
        &[
            "clone",
            "-q",
            "--depth=1",
            "--no-local",
            source.repository.to_str().unwrap(),
            shallow.repository.to_str().unwrap(),
        ],
    );
    let workspace = shallow.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let mut runner = UnshallowDuringPruneRunner {
        inner: shallow.runner(&workspace),
        repository: shallow.repository.clone(),
        unshallowed: false,
    };
    assert!(matches!(
        test_support::refresh_with_runner(
            &mut provider,
            &workspace,
            &index,
            &request(),
            &HistoryOptions::default(),
            &mut runner,
        ),
        Err(HistoryError::Malformed(_))
    ));
    assert!(runner.unshallowed);
    assert!(provider.graph().is_none());
}

#[test]
fn shallow_history_is_observed_partial() {
    let source = Fixture::pinned();
    let mut shallow = Fixture::empty("shallow-placeholder");
    fs::remove_dir_all(&shallow.repository).unwrap();
    git(
        &shallow.root,
        &[
            "clone",
            "-q",
            "--depth=1",
            "--no-local",
            source.repository.to_str().unwrap(),
            shallow.repository.to_str().unwrap(),
        ],
    );
    shallow.commits = vec![source.commits.last().unwrap().clone()];
    let workspace = shallow.open();
    let index = build_index(&workspace);
    let mut provider = HistoryGraphProvider::new();
    let graph = provider
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap();
    assert_eq!(graph.commits().len(), 1);
    assert!(graph.coverage().iter().any(|item| {
        item.area() == CoverageArea::Commits && item.status() == CoverageStatus::ObservedPartial
    }));
    for area in [
        CoverageArea::Commits,
        CoverageArea::Renames,
        CoverageArea::CoChange,
        CoverageArea::Blame,
    ] {
        let records = graph
            .coverage()
            .iter()
            .filter(|item| item.area() == area)
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status(), CoverageStatus::ObservedPartial);
    }
    let shallow_digest = graph.shallow_digest();
    git(
        &shallow.repository,
        &["fetch", "-q", "--unshallow", "origin"],
    );
    assert!(provider.validated_graph(&workspace).is_err());
    let index = build_index(&workspace);
    let refreshed = provider
        .refresh(&workspace, &index, &request(), &HistoryOptions::default())
        .unwrap();
    assert_ne!(refreshed.shallow_digest(), shallow_digest);
    assert!(refreshed.coverage().iter().any(|record| {
        record.area() == CoverageArea::Commits && record.status() == CoverageStatus::Complete
    }));
}
