#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{collections::BTreeSet, fs, path::PathBuf, time::Duration};

use kit::{
    domain::{
        events::ContentDigest,
        ids::{PrincipalId, ProcessId, ProjectId, WorkspaceId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    },
    executor::profile::{
        Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
    },
    verify::lsp::{
        facts::{
            FactLimits, LspWorkspaceSnapshot, OpenDocument, SemanticFact, SnapshotFile,
            normalize_semantic_locations,
        },
        session::{
            AcceptedResponse, CodecLimits, DocumentVersion, ExecutionProfileIdentity,
            LaunchRequest, LspCodec, LspSessionManager, OwnedLspLauncher, OwnedLspTransport,
            PositionEncoding, ResponseDisposition, RevisionPolicy, SendContext, ServerIdentity,
            SessionLimits, SessionPurpose, SessionScope, TransportError,
        },
    },
    workspace::{
        edit::ir::EditLimits,
        edit::ir::RootRelativePath,
        index::meta::{IndexOptions, MetadataIndex},
        map::{
            DeclarationId, ExpansionPurpose, ExpansionRequest, MAP_CURSOR_TOKEN_LENGTH, MapBound,
            MapBudget, MapCursor, MapError, MapLimits, Personalization, RelationshipKind,
            RepositoryMap, RepositoryMapRequest, ScoreBand, SemanticRelationship, StackFrame,
            build_repository_map,
        },
        revision::{ManagedWorkspace, RevisionId, RevisionOptions},
    },
};
use serde_json::json;
use url::Url;

struct Fixture {
    parent: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut random = [0_u8; 12];
        getrandom::fill(&mut random).unwrap();
        let parent = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-repo-map-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let root = parent.join("workspace");
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { parent, root };
        for (path, source) in files {
            fixture.write(path, source);
        }
        fixture
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, source).unwrap();
    }

    fn open(&self) -> ManagedWorkspace {
        ManagedWorkspace::open_with_options(
            &self.root,
            RevisionOptions {
                max_entries: 10_000,
                max_name_bytes: 8 * 1024 * 1024,
                max_bytes: 64 * 1024 * 1024,
                max_memory_bytes: 128 * 1024 * 1024,
                max_depth: 128,
                max_scan_time: Duration::from_secs(10),
                max_scan_attempts: 2,
                watcher_interval: Duration::from_millis(5),
                reconciliation_interval: Duration::from_secs(60),
                metadata_path: Some(self.parent.join("revision.state")),
            },
        )
        .unwrap()
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

fn indexed(fixture: &Fixture) -> (ManagedWorkspace, MetadataIndex) {
    indexed_with_options(fixture, &IndexOptions::default())
}

fn indexed_with_options(
    fixture: &Fixture,
    options: &IndexOptions,
) -> (ManagedWorkspace, MetadataIndex) {
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let index = MetadataIndex::build(&workspace, revision, options).unwrap();
    (workspace, index)
}

fn map(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    request: &RepositoryMapRequest,
) -> RepositoryMap {
    build_repository_map(workspace, index, request, &[], MapLimits::default(), None).unwrap()
}

fn wire(map: &RepositoryMap) -> serde_json::Value {
    serde_json::from_slice(&map.to_canonical_json().unwrap()).unwrap()
}

fn wire_id(id: DeclarationId) -> serde_json::Value {
    serde_json::to_value(id).unwrap()
}

fn id(index: &MetadataIndex, name: &str) -> DeclarationId {
    index
        .entries()
        .iter()
        .flat_map(|entry| entry.syntax_records.iter())
        .find(|record| record.qualified_name().value().as_str() == name)
        .map(|record| DeclarationId::from(record.declaration_id()))
        .unwrap()
}

#[test]
fn five_hard_budget_bounds_are_exact_and_the_ceiling_is_valid() {
    let fixture = Fixture::new(&[("lib.rs", "fn alpha() {}\nfn beta() {}\n")]);
    let (workspace, index) = indexed(&fixture);
    let limits = MapLimits::default();

    for (budget, expected) in [
        (
            MapBudget {
                max_items: limits.max_items + 1,
                ..MapBudget::default()
            },
            MapBound::Items,
        ),
        (
            MapBudget {
                max_estimated_tokens: limits.max_estimated_tokens + 1,
                ..MapBudget::default()
            },
            MapBound::EstimatedTokens,
        ),
        (
            MapBudget {
                max_hops: limits.max_hops + 1,
                ..MapBudget::default()
            },
            MapBound::Hops,
        ),
        (
            MapBudget {
                max_degree: limits.max_degree + 1,
                ..MapBudget::default()
            },
            MapBound::Degree,
        ),
        (
            MapBudget {
                max_result_bytes: limits.max_result_bytes + 1,
                ..MapBudget::default()
            },
            MapBound::ResultBytes,
        ),
    ] {
        let request = RepositoryMapRequest {
            budget,
            ..RepositoryMapRequest::default()
        };
        assert!(matches!(
            build_repository_map(&workspace, &index, &request, &[], limits, None),
            Err(MapError::BoundExceeded(bound)) if bound == expected
        ));
    }

    let request = RepositoryMapRequest {
        budget: MapBudget {
            max_items: limits.max_items,
            max_estimated_tokens: limits.max_estimated_tokens,
            max_hops: limits.max_hops,
            max_degree: limits.max_degree,
            max_result_bytes: limits.max_result_bytes,
        },
        ..RepositoryMapRequest::default()
    };
    let output = build_repository_map(&workspace, &index, &request, &[], limits, None).unwrap();
    assert!(output.item_count() <= request.budget.max_items);
    assert!(output.estimated_tokens() <= request.budget.max_estimated_tokens);
    assert!(
        wire(&output)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| {
                entry["hops"]
                    .as_u64()
                    .is_none_or(|hops| hops <= request.budget.max_hops as u64)
                    && entry["degree"].as_u64().unwrap() <= request.budget.max_degree as u64
            })
    );
    assert!(output.result_bytes() <= request.budget.max_result_bytes);
}

#[test]
fn personalization_is_deterministic_set_like_and_recency_is_ordered() {
    let fixture = Fixture::new(&[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]);
    let (workspace, index) = indexed(&fixture);
    let alpha = id(&index, "alpha");
    let beta = id(&index, "beta");
    let mut request = RepositoryMapRequest {
        personalization: Personalization {
            task_terms: vec!["zzz".to_owned(), "beta".to_owned()],
            exact_declaration_ids: vec![alpha],
            stack_frames: vec![StackFrame {
                path: "b.rs".into(),
                symbol: Some("beta".to_owned()),
                line: Some(1),
            }],
            recently_read_paths: Vec::new(),
            current_edit_paths: vec!["a.rs".into()],
        },
        ..RepositoryMapRequest::default()
    };
    let first = map(&workspace, &index, &request);
    let repeated = map(&workspace, &index, &request);
    assert_eq!(
        first.to_canonical_json().unwrap(),
        repeated.to_canonical_json().unwrap()
    );
    assert_eq!(wire(&first)["entries"][0]["declaration_id"], wire_id(alpha));

    request.personalization.task_terms.reverse();
    request.personalization.exact_declaration_ids.push(alpha);
    request
        .personalization
        .current_edit_paths
        .push("a.rs".into());
    assert_eq!(
        first.to_canonical_json().unwrap(),
        map(&workspace, &index, &request)
            .to_canonical_json()
            .unwrap()
    );

    request.personalization = Personalization {
        recently_read_paths: vec!["a.rs".into(), "b.rs".into()],
        ..Personalization::default()
    };
    assert_eq!(
        wire(&map(&workspace, &index, &request))["entries"][0]["declaration_id"],
        wire_id(alpha)
    );
    request.personalization.recently_read_paths.reverse();
    assert_eq!(
        wire(&map(&workspace, &index, &request))["entries"][0]["declaration_id"],
        wire_id(beta)
    );
}

#[test]
fn nested_containment_expands_both_directions_and_runtime_bounds_are_atomic() {
    let fixture = Fixture::new(&[(
        "lib.rs",
        "mod outer { fn parent() { fn left() {} fn right() {} } }\n",
    )]);
    let (workspace, index) = indexed(&fixture);
    let outer = id(&index, "outer");
    let parent = id(&index, "outer::parent");
    let left = id(&index, "outer::parent::left");

    let mut request = RepositoryMapRequest::default();
    request.expansion.seeds = vec![outer];
    request.expansion.relationships = vec![RelationshipKind::Contains];
    let forward = map(&workspace, &index, &request);
    assert!(
        wire(&forward)["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| {
                edge["source_declaration"] == wire_id(outer)
                    && edge["target_declaration"] == wire_id(parent)
                    && edge["relationship"] == "contains"
            })
    );
    assert!(
        wire(&forward)["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["target_declaration"] == wire_id(left))
    );

    request.expansion.seeds = vec![left];
    request.expansion.relationships = vec![RelationshipKind::ContainedBy];
    let reverse = map(&workspace, &index, &request);
    assert!(
        wire(&reverse)["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| {
                edge["source_declaration"] == wire_id(left)
                    && edge["target_declaration"] == wire_id(parent)
                    && edge["relationship"] == "contained_by"
            })
    );

    request.expansion.seeds = vec![outer];
    request.expansion.relationships = vec![RelationshipKind::Contains];
    request.budget.max_hops = 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Hops))
    ));
    request.budget.max_hops = 4;
    request.budget.max_degree = 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Degree))
    ));
    request.budget.max_degree = 64;
    request.budget.max_items = 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Items))
    ));
}

#[test]
fn every_expansion_selector_is_exact_inclusive_mandatory_and_set_like() {
    let fixture = Fixture::new(&[
        ("a.rs", "mod alpha { fn parent() { fn child() {} } }\n"),
        ("b.rs", "mod beta { fn parent() { fn child() {} } }\n"),
    ]);
    let (workspace, index) = indexed(&fixture);
    let alpha = id(&index, "alpha");
    let alpha_parent = id(&index, "alpha::parent");
    let alpha_child = id(&index, "alpha::parent::child");
    let beta_parent = id(&index, "beta::parent");
    let beta_child = id(&index, "beta::parent::child");

    let mut path_request = RepositoryMapRequest::default();
    path_request.expansion.paths = vec![RootRelativePath::parse("a.rs", 4096).unwrap()];
    path_request.expansion.relationships = vec![RelationshipKind::Contains];
    let path = wire(&map(&workspace, &index, &path_request));
    assert!(path["edges"].as_array().unwrap().iter().any(|edge| {
        edge["source_declaration"] == wire_id(alpha_parent)
            && edge["target_declaration"] == wire_id(alpha_child)
    }));
    assert!(
        path["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| { entry["path"] != "a.rs" || entry["hops"].as_u64() == Some(0) })
    );

    let mut qualified_request = RepositoryMapRequest::default();
    qualified_request.expansion.symbols = vec!["alpha::parent".to_owned()];
    qualified_request.expansion.relationships = vec![RelationshipKind::Contains];
    let qualified = wire(&map(&workspace, &index, &qualified_request));
    assert!(qualified["edges"].as_array().unwrap().iter().any(|edge| {
        edge["source_declaration"] == wire_id(alpha_parent)
            && edge["target_declaration"] == wire_id(alpha_child)
    }));
    assert_eq!(
        qualified["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["declaration_id"] == wire_id(alpha_parent))
            .unwrap()["hops"],
        0
    );

    let mut display_request = RepositoryMapRequest::default();
    display_request.expansion.symbols = vec!["parent".to_owned()];
    display_request.expansion.relationships = vec![RelationshipKind::Contains];
    let display = wire(&map(&workspace, &index, &display_request));
    for (parent, child) in [(alpha_parent, alpha_child), (beta_parent, beta_child)] {
        assert!(display["edges"].as_array().unwrap().iter().any(|edge| {
            edge["source_declaration"] == wire_id(parent)
                && edge["target_declaration"] == wire_id(child)
        }));
    }

    let ranked_request = RepositoryMapRequest {
        personalization: Personalization {
            exact_declaration_ids: vec![alpha],
            ..Personalization::default()
        },
        ..RepositoryMapRequest::default()
    };
    let ranked = wire(&map(&workspace, &index, &ranked_request));
    let alpha_rank = ranked["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["declaration_id"] == wire_id(alpha))
        .unwrap()["rank"]
        .as_u64()
        .unwrap();
    let score_request = RepositoryMapRequest {
        personalization: ranked_request.personalization.clone(),
        expansion: ExpansionRequest {
            score_band: Some(ScoreBand {
                min: alpha_rank,
                max: alpha_rank,
            }),
            relationships: vec![RelationshipKind::Contains],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    let score = wire(&map(&workspace, &index, &score_request));
    assert_eq!(
        score["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["hops"].as_u64() == Some(0))
            .count(),
        1
    );
    assert!(score["edges"].as_array().unwrap().iter().any(|edge| {
        edge["source_declaration"] == wire_id(alpha)
            && edge["target_declaration"] == wire_id(alpha_parent)
    }));

    let seed_only = RepositoryMapRequest {
        expansion: ExpansionRequest {
            seeds: vec![alpha],
            relationships: vec![RelationshipKind::Contains],
            ..ExpansionRequest::default()
        },
        personalization: ranked_request.personalization.clone(),
        ..RepositoryMapRequest::default()
    };
    let union = RepositoryMapRequest {
        expansion: ExpansionRequest {
            seeds: vec![alpha, alpha],
            symbols: vec!["alpha".to_owned(), "alpha".to_owned()],
            score_band: Some(ScoreBand {
                min: alpha_rank,
                max: alpha_rank,
            }),
            relationships: vec![RelationshipKind::Contains],
            ..ExpansionRequest::default()
        },
        ..seed_only.clone()
    };
    let seed_only = wire(&map(&workspace, &index, &seed_only));
    let union = wire(&map(&workspace, &index, &union));
    assert_eq!(seed_only["entries"], union["entries"]);
    assert_eq!(seed_only["edges"], union["edges"]);

    let empty = wire(&map(&workspace, &index, &RepositoryMapRequest::default()));
    assert!(
        empty["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["hops"].is_null())
    );
}

#[test]
fn repository_tree_expands_indexed_files_and_directories_without_declarations() {
    let fixture = Fixture::new(&[
        (".gitignore", "ignored.rs\n"),
        (".kit/private.rs", "fn private_item() {}\n"),
        ("Cargo.toml", "[package]\nname = \"fixture\"\n"),
        ("ignored.rs", "fn ignored_item() {}\n"),
        ("src/lib.rs", "fn library_item() {}\n"),
        ("src/other.rs", "fn other_item() {}\n"),
        ("src/nested/mod.rs", "fn nested_item() {}\n"),
        ("src2/lib.rs", "fn sibling_item() {}\n"),
    ]);
    let (workspace, index) = indexed(&fixture);
    let indexed_paths = index
        .entries()
        .iter()
        .map(|entry| entry.path.to_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(indexed_paths.contains("src"));
    assert!(indexed_paths.contains("Cargo.toml"));
    assert!(!indexed_paths.contains("ignored.rs"));
    assert!(!indexed_paths.contains(".kit/private.rs"));

    let cargo_request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("Cargo.toml", 4096).unwrap()],
            relationships: Vec::new(),
            ..ExpansionRequest::default()
        },
        path_prefixes: vec!["Cargo.toml".into()],
        ..RepositoryMapRequest::default()
    };
    let cargo = wire(&map(&workspace, &index, &cargo_request));
    assert!(cargo["entries"].as_array().unwrap().is_empty());
    assert!(cargo["path_edges"].as_array().unwrap().is_empty());
    assert_eq!(cargo["path_nodes"].as_array().unwrap().len(), 1);
    let cargo_node = &cargo["path_nodes"][0];
    assert_eq!(cargo_node["path"], "Cargo.toml");
    assert_eq!(cargo_node["kind"], "file");
    assert_eq!(cargo_node["content_state"], "text");
    assert_eq!(cargo_node["hops"], 0);
    assert_eq!(cargo_node["revision"], index.revision().to_string());
    assert_eq!(cargo_node["provenance"]["classification"], "syntactic");
    assert_eq!(cargo_node["provenance"]["source"], "repository_tree");
    assert_eq!(
        cargo_node["provenance"]["revision"],
        index.revision().to_string()
    );
    assert_eq!(
        cargo_node["size"].as_u64(),
        Some("[package]\nname = \"fixture\"\n".len() as u64)
    );

    let declaration_file_request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("src/lib.rs", 4096).unwrap()],
            relationships: Vec::new(),
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    let declaration_file = wire(&map(&workspace, &index, &declaration_file_request));
    assert_eq!(declaration_file["path_nodes"][0]["path"], "src/lib.rs");
    assert!(
        declaration_file["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["qualified_name"] == "library_item" && entry["hops"] == 0 })
    );

    let directory_request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("src", 4096).unwrap()],
            purpose: ExpansionPurpose::Dependencies,
            relationships: vec![RelationshipKind::Contains],
            ..ExpansionRequest::default()
        },
        path_prefixes: vec!["Cargo.toml".into()],
        ..RepositoryMapRequest::default()
    };
    let directory_map = map(&workspace, &index, &directory_request);
    let directory = wire(&directory_map);
    let paths = directory["path_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        paths,
        BTreeSet::from([
            "src",
            "src/lib.rs",
            "src/nested",
            "src/nested/mod.rs",
            "src/other.rs",
        ])
    );
    assert!(!paths.contains("src2"));
    assert!(
        directory["path_edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| {
                edge["source_path"] == "src"
                    && edge["target_path"] == "src/nested"
                    && edge["relationship"] == "contains"
                    && edge["hops"] == 1
                    && edge["provenance"]["source"] == "repository_tree"
            })
    );
    assert!(
        directory["path_edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| {
                !(edge["source_path"] == "src" && edge["target_path"] == "src/nested/mod.rs")
            })
    );

    let dependent_request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("src/lib.rs", 4096).unwrap()],
            purpose: ExpansionPurpose::Dependents,
            relationships: vec![RelationshipKind::ContainedBy],
            ..ExpansionRequest::default()
        },
        path_prefixes: vec!["Cargo.toml".into()],
        ..RepositoryMapRequest::default()
    };
    let dependent = wire(&map(&workspace, &index, &dependent_request));
    assert_eq!(dependent["path_nodes"].as_array().unwrap().len(), 2);
    assert_eq!(dependent["path_edges"].as_array().unwrap().len(), 1);
    assert_eq!(dependent["path_edges"][0]["source_path"], "src/lib.rs");
    assert_eq!(dependent["path_edges"][0]["target_path"], "src");
    assert_eq!(dependent["path_edges"][0]["relationship"], "contained_by");

    let required_items = directory_map.item_count();
    let required_tokens = directory_map.estimated_tokens();
    assert_eq!(
        required_items,
        directory["path_nodes"].as_array().unwrap().len()
            + directory["path_edges"].as_array().unwrap().len()
    );
    let mut bounded = directory_request.clone();
    bounded.budget.max_items = required_items;
    bounded.budget.max_estimated_tokens = required_tokens;
    assert_eq!(
        map(&workspace, &index, &bounded).item_count(),
        required_items
    );
    bounded.budget.max_estimated_tokens = required_tokens - 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &bounded,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::EstimatedTokens))
    ));
    bounded.budget.max_estimated_tokens = required_tokens;
    bounded.budget.max_items = required_items - 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &bounded,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Items))
    ));
    bounded.budget.max_items = required_items;
    bounded.budget.max_hops = 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &bounded,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Hops))
    ));
    bounded.budget.max_hops = 4;
    bounded.budget.max_degree = 2;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &bounded,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Degree))
    ));

    let missing = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("sr", 4096).unwrap()],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &missing,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::SelectorNoMatch("path"))
    ));
}

#[test]
fn path_mandatory_items_appear_once_before_ranked_cursor_pages() {
    let fixture = Fixture::new(&[
        ("Cargo.toml", "[package]\nname = \"fixture\"\n"),
        ("a.rs", "fn alpha() {}\n"),
        ("b.rs", "fn beta() {}\n"),
    ]);
    let (workspace, index) = indexed(&fixture);
    let request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("Cargo.toml", 4096).unwrap()],
            relationships: Vec::new(),
            ..ExpansionRequest::default()
        },
        budget: MapBudget {
            max_items: 1,
            ..MapBudget::default()
        },
        ..RepositoryMapRequest::default()
    };

    let first = build_repository_map(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        None,
    )
    .unwrap();
    let first_wire = wire(&first);
    assert_eq!(first.item_count(), 1);
    assert_eq!(first_wire["path_nodes"][0]["path"], "Cargo.toml");
    assert!(first_wire["entries"].as_array().unwrap().is_empty());

    let second = build_repository_map(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        first.cursor(),
    )
    .unwrap();
    let second_wire = wire(&second);
    assert!(second_wire["path_nodes"].as_array().unwrap().is_empty());
    assert!(second_wire["path_edges"].as_array().unwrap().is_empty());
    assert_eq!(second_wire["entries"].as_array().unwrap().len(), 1);
    assert_ne!(second.cursor(), first.cursor());
}

#[test]
fn malformed_and_unmatched_expansion_selectors_are_rejected() {
    let fixture = Fixture::new(&[("lib.rs", "fn alpha() {}\n")]);
    let (workspace, index) = indexed(&fixture);
    assert!(RootRelativePath::parse("../lib.rs", 4096).is_err());

    for request in [
        RepositoryMapRequest {
            expansion: ExpansionRequest {
                paths: vec![RootRelativePath::parse("missing.rs", 4096).unwrap()],
                ..ExpansionRequest::default()
            },
            ..RepositoryMapRequest::default()
        },
        RepositoryMapRequest {
            expansion: ExpansionRequest {
                symbols: vec!["alph".to_owned()],
                ..ExpansionRequest::default()
            },
            ..RepositoryMapRequest::default()
        },
        RepositoryMapRequest {
            expansion: ExpansionRequest {
                score_band: Some(ScoreBand {
                    min: u64::MAX,
                    max: u64::MAX,
                }),
                ..ExpansionRequest::default()
            },
            ..RepositoryMapRequest::default()
        },
    ] {
        assert!(matches!(
            build_repository_map(
                &workspace,
                &index,
                &request,
                &[],
                MapLimits::default(),
                None
            ),
            Err(MapError::SelectorNoMatch(_))
        ));
    }

    let malformed = RepositoryMapRequest {
        expansion: ExpansionRequest {
            score_band: Some(ScoreBand { min: 2, max: 1 }),
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &malformed,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::InvalidRequest(
            "expansion score band minimum exceeds maximum"
        ))
    ));

    let too_many_paths = RepositoryMapRequest {
        expansion: ExpansionRequest {
            paths: vec![RootRelativePath::parse("lib.rs", 4096).unwrap(); 2],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &too_many_paths,
            &[],
            MapLimits {
                max_expansion_paths: 1,
                ..MapLimits::default()
            },
            None
        ),
        Err(MapError::InvalidRequest("too many expansion paths"))
    ));
    let oversized_symbol = RepositoryMapRequest {
        expansion: ExpansionRequest {
            symbols: vec!["alpha".to_owned()],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &oversized_symbol,
            &[],
            MapLimits {
                max_input_bytes: 1,
                ..MapLimits::default()
            },
            None
        ),
        Err(MapError::InvalidRequest("input byte limit exceeded"))
    ));
}

#[test]
fn changing_any_expansion_selector_invalidates_the_cursor() {
    let fixture = Fixture::new(&[
        ("a.rs", "fn alpha() {}\n"),
        ("b.rs", "fn beta() {}\nfn gamma() {}\n"),
    ]);
    let (workspace, index) = indexed(&fixture);
    let request = RepositoryMapRequest {
        budget: MapBudget {
            max_items: 1,
            ..MapBudget::default()
        },
        ..RepositoryMapRequest::default()
    };
    let first = map(&workspace, &index, &request);
    let cursor = first.cursor().unwrap();

    let mut changed = Vec::new();
    let mut declaration = request.clone();
    declaration.expansion.seeds = vec![id(&index, "alpha")];
    changed.push(declaration);
    let mut path = request.clone();
    path.expansion.paths = vec![RootRelativePath::parse("a.rs", 4096).unwrap()];
    changed.push(path);
    let mut symbol = request.clone();
    symbol.expansion.symbols = vec!["alpha".to_owned()];
    changed.push(symbol);
    let mut score = request.clone();
    score.expansion.score_band = Some(ScoreBand {
        min: 0,
        max: u64::MAX,
    });
    changed.push(score);
    let mut relationship = request.clone();
    relationship.expansion.relationships.clear();
    changed.push(relationship);

    for changed in changed {
        assert!(matches!(
            build_repository_map(
                &workspace,
                &index,
                &changed,
                &[],
                MapLimits::default(),
                Some(cursor)
            ),
            Err(MapError::CursorMismatch)
        ));
    }
}

#[test]
fn canonical_json_accounting_unicode_and_nonresumable_truncation_are_exact() {
    let fixture = Fixture::new(&[(
        "unicode.rs",
        "fn café() { let text = \"quoted \\\" value\"; }\nfn extra() {}\n",
    )]);
    let (workspace, index) = indexed(&fixture);
    let output = map(&workspace, &index, &RepositoryMapRequest::default());
    let bytes = output.to_canonical_json().unwrap();
    assert_eq!(output.result_bytes(), bytes.len());
    assert_eq!(output.estimated_tokens(), bytes.len().div_ceil(4));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.to_string().contains("café"));
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("\\\"quoted \\\\\\\" value\\\"")
    );

    let (limited_workspace, limited_index) = indexed_with_options(
        &fixture,
        &IndexOptions {
            max_symbols_per_file: 1,
            ..IndexOptions::default()
        },
    );
    let limited = map(
        &limited_workspace,
        &limited_index,
        &RepositoryMapRequest::default(),
    );
    assert!(limited.truncated());
    assert!(limited.cursor().is_none());
}

#[test]
fn ranked_cursor_and_workspace_revision_are_fenced() {
    let fixture = Fixture::new(&[("lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n")]);
    let (workspace, index) = indexed(&fixture);
    let request = RepositoryMapRequest {
        budget: MapBudget {
            max_items: 1,
            ..MapBudget::default()
        },
        ..RepositoryMapRequest::default()
    };
    let first = map(&workspace, &index, &request);
    let cursor = first
        .cursor()
        .expect("complete ranked frontier is resumable");
    let token = cursor.to_token();
    assert_eq!(token.len(), MAP_CURSOR_TOKEN_LENGTH);
    assert_eq!(MapCursor::from_token(&token).unwrap(), cursor.clone());
    assert!(serde_json::to_value(cursor).unwrap().is_string());
    let mut tampered = token.clone().into_bytes();
    let last = tampered.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    assert!(MapCursor::from_token(std::str::from_utf8(&tampered).unwrap()).is_err());
    assert!(MapCursor::from_token(&token.to_uppercase()).is_err());
    assert!(MapCursor::from_token(&(token.clone() + "00")).is_err());
    let decoded = MapCursor::from_token(&token).unwrap();
    let second = build_repository_map(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        Some(&decoded),
    )
    .unwrap();
    assert_ne!(
        wire(&first)["entries"][0]["declaration_id"],
        wire(&second)["entries"][0]["declaration_id"]
    );
    let mut seen = BTreeSet::from([wire(&first)["entries"][0]["declaration_id"]
        .as_str()
        .unwrap()
        .to_owned()]);
    assert!(
        seen.insert(
            wire(&second)["entries"][0]["declaration_id"]
                .as_str()
                .unwrap()
                .to_owned()
        )
    );
    let third = build_repository_map(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        second.cursor(),
    )
    .unwrap();
    assert!(
        seen.insert(
            wire(&third)["entries"][0]["declaration_id"]
                .as_str()
                .unwrap()
                .to_owned()
        )
    );
    assert!(third.cursor().is_none());
    let changed_request = RepositoryMapRequest {
        personalization: Personalization {
            task_terms: vec!["two".to_owned()],
            ..Personalization::default()
        },
        ..request.clone()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &changed_request,
            &[],
            MapLimits::default(),
            Some(cursor)
        ),
        Err(MapError::CursorMismatch)
    ));

    fixture.write("lib.rs", "fn replacement() {}\n");
    let next_revision = workspace.current_revision().unwrap().id();
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            Some(cursor)
        ),
        Err(MapError::Revision(_))
    ));
    let next_index =
        MetadataIndex::build(&workspace, next_revision, &IndexOptions::default()).unwrap();
    assert!(matches!(
        build_repository_map(
            &workspace,
            &next_index,
            &request,
            &[],
            MapLimits::default(),
            Some(cursor)
        ),
        Err(MapError::CursorMismatch)
    ));
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

fn digest(byte: u8) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", format!("{byte:02x}").repeat(32))).unwrap()
}

fn server() -> ServerIdentity {
    ServerIdentity {
        server_artifact: digest(1),
        configuration: digest(2),
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

fn accepted_definition(
    fixture: &Fixture,
    revision: RevisionId,
    source: &str,
    origin_line: usize,
    target_line: usize,
    target_start: usize,
    target_end: usize,
) -> AcceptedResponse {
    accepted_definition_paths(
        fixture,
        revision,
        "lib.rs",
        source,
        origin_line,
        "lib.rs",
        target_line,
        target_start,
        target_end,
        PositionEncoding::Utf8,
    )
}

#[allow(clippy::too_many_arguments)]
fn accepted_definition_paths(
    fixture: &Fixture,
    revision: RevisionId,
    source_path: &str,
    source: &str,
    origin_line: usize,
    target_path: &str,
    target_line: usize,
    target_start: usize,
    target_end: usize,
    encoding: PositionEncoding,
) -> AcceptedResponse {
    let uri = fixture.uri(source_path);
    let target_uri = fixture.uri(target_path);
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
            uri.clone(),
            DocumentVersion::new(1),
            source.to_owned(),
        )
        .unwrap();
    let token = manager
        .request(
            service,
            revision,
            &uri,
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": origin_line, "character": 0}
            }),
            manager.now_tick() + 10_000,
        )
        .unwrap();
    let frame = LspCodec::encode(
        &json!({
            "jsonrpc":"2.0",
            "id":token.request_id.get(),
            "result":{
                "uri":target_uri,
                "range":{
                    "start":{"line":target_line,"character":target_start},
                    "end":{"line":target_line,"character":target_end}
                }
            }
        }),
        SessionLimits::default().codec,
    )
    .unwrap();
    let ResponseDisposition::Accepted(accepted) = manager
        .receive_captured_response(service, &token, &frame)
        .unwrap()
    else {
        panic!("current response was not accepted");
    };
    manager.shutdown().unwrap();
    accepted
}

fn semantic_definition(
    fixture: &Fixture,
    revision: RevisionId,
    source_path: &str,
    source: &str,
    target_path: &str,
    target: &str,
    encoding: PositionEncoding,
) -> Vec<SemanticFact> {
    let snapshot = LspWorkspaceSnapshot::new(
        fixture.root.clone(),
        revision,
        1,
        vec![
            SnapshotFile::new(source_path, source.as_bytes().to_vec(), false),
            SnapshotFile::new(target_path, target.as_bytes().to_vec(), false),
        ],
        vec![OpenDocument::new(
            fixture.uri(source_path),
            DocumentVersion::new(1),
            source.to_owned(),
        )],
        server(),
        encoding,
        EditLimits::default(),
        FactLimits::default(),
    )
    .unwrap();
    let accepted = accepted_definition_paths(
        fixture,
        revision,
        source_path,
        source,
        0,
        target_path,
        0,
        3,
        9,
        encoding,
    );
    normalize_semantic_locations(&snapshot, &accepted).unwrap()
}

#[test]
fn only_normalized_semantic_evidence_creates_semantic_edges_and_cycles_terminate() {
    let source = "fn source() { target(); }\nfn target() { source(); }\n";
    let fixture = Fixture::new(&[("lib.rs", source)]);
    let (workspace, index) = indexed(&fixture);
    let revision = index.revision();
    let source_id = id(&index, "source");
    let target_id = id(&index, "target");
    let snapshot = LspWorkspaceSnapshot::new(
        fixture.root.clone(),
        revision,
        1,
        vec![SnapshotFile::new(
            "lib.rs",
            source.as_bytes().to_vec(),
            false,
        )],
        vec![OpenDocument::new(
            fixture.uri("lib.rs"),
            DocumentVersion::new(1),
            source.to_owned(),
        )],
        server(),
        PositionEncoding::Utf8,
        EditLimits::default(),
        FactLimits::default(),
    )
    .unwrap();
    let to_target = accepted_definition(&fixture, revision, source, 0, 1, 3, 9);
    let to_source = accepted_definition(&fixture, revision, source, 1, 0, 3, 9);
    let target_facts = normalize_semantic_locations(&snapshot, &to_target).unwrap();
    let source_facts = normalize_semantic_locations(&snapshot, &to_source).unwrap();

    let mut request = RepositoryMapRequest::default();
    request.expansion.seeds = vec![source_id];
    request.expansion.purpose = ExpansionPurpose::Neighborhood;
    request.expansion.relationships = vec![RelationshipKind::SemanticDefinition];
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::SemanticEvidenceUnavailable)
    ));

    let evidence = [
        SemanticRelationship::new(source_id, &target_facts[0]),
        SemanticRelationship::new(target_id, &source_facts[0]),
    ];
    let output = build_repository_map(
        &workspace,
        &index,
        &request,
        &evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    let permuted = build_repository_map(
        &workspace,
        &index,
        &request,
        &[evidence[1], evidence[0]],
        MapLimits::default(),
        None,
    )
    .unwrap();
    assert_eq!(
        output.to_canonical_json().unwrap(),
        permuted.to_canonical_json().unwrap()
    );
    let output_wire = wire(&output);
    assert_eq!(output_wire["entries"].as_array().unwrap().len(), 2);
    assert_eq!(output_wire["edges"].as_array().unwrap().len(), 2);
    assert!(output_wire["edges"].as_array().unwrap().iter().all(|edge| {
        edge["relationship"] == "semantic_definition" && edge["provenance"]["source"] == "lsp"
    }));

    let relabeled_same_file = [SemanticRelationship::new(target_id, &target_facts[0])];
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &relabeled_same_file,
            MapLimits::default(),
            None
        ),
        Err(MapError::InvalidFact(
            "semantic source declaration does not contain request origin"
        ))
    ));

    fixture.write("lib.rs", "fn replacement() {}\n");
    let next_revision = workspace.current_revision().unwrap().id();
    let next_index =
        MetadataIndex::build(&workspace, next_revision, &IndexOptions::default()).unwrap();
    build_repository_map(
        &workspace,
        &next_index,
        &RepositoryMapRequest::default(),
        &evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    let semantic_request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            relationships: vec![RelationshipKind::SemanticDefinition],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &next_index,
            &semantic_request,
            &evidence,
            MapLimits::default(),
            None
        ),
        Err(MapError::StaleFact)
    ));
}

#[test]
fn runtime_item_token_hop_and_degree_bounds_are_exact() {
    let fixture = Fixture::new(&[(
        "lib.rs",
        "mod outer { fn parent() { fn left() {} fn right() {} } }\n",
    )]);
    let (workspace, index) = indexed(&fixture);
    let outer = id(&index, "outer");

    let mut request = RepositoryMapRequest::default();
    request.expansion.seeds = vec![outer];
    request.expansion.relationships = vec![RelationshipKind::Contains];
    request.budget.max_hops = 2;
    request.budget.max_degree = 2;
    let exact = map(&workspace, &index, &request);
    let required_items = exact.item_count();
    let required_tokens = exact.estimated_tokens();
    assert!(required_items > 1);
    assert!(required_tokens > 1);
    assert_eq!(
        wire(&exact)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["hops"].as_u64())
            .max(),
        Some(2)
    );
    assert_eq!(
        wire(&exact)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["degree"].as_u64())
            .max(),
        Some(2)
    );

    request.budget.max_items = required_items;
    request.budget.max_estimated_tokens = required_tokens;
    let at_exact = map(&workspace, &index, &request);
    assert_eq!(at_exact.item_count(), required_items);
    assert_eq!(at_exact.estimated_tokens(), required_tokens);

    request.budget.max_estimated_tokens = required_tokens - 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::EstimatedTokens))
    ));
    request.budget.max_estimated_tokens = required_tokens;
    request.budget.max_items = required_items - 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Items))
    ));
    request.budget.max_items = required_items;
    request.budget.max_hops = 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Hops))
    ));
    request.budget.max_hops = 2;
    request.budget.max_degree = 1;
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::Degree))
    ));
}

#[test]
fn mandatory_only_first_page_resumes_ranked_union_without_duplicates() {
    let fixture = Fixture::new(&[("lib.rs", "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n")]);
    let (workspace, index) = indexed(&fixture);
    let mut request = RepositoryMapRequest::default();
    request.expansion.seeds = vec![id(&index, "alpha")];
    request.expansion.relationships.clear();
    request.budget.max_items = 1;

    let mut cursor = None;
    let mut declarations = BTreeSet::new();
    let mut edges = BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            cursor.as_ref(),
        )
        .unwrap();
        pages += 1;
        assert!(page.item_count() > 0 || !page.truncated());
        let page_wire = wire(&page);
        for entry in page_wire["entries"].as_array().unwrap() {
            assert!(declarations.insert(entry["declaration_id"].as_str().unwrap().to_owned()));
        }
        for edge in page_wire["edges"].as_array().unwrap() {
            assert!(edges.insert((
                edge["source_declaration"].as_str().unwrap().to_owned(),
                edge["target_declaration"].as_str().unwrap().to_owned(),
                edge["relationship"].as_str().unwrap().to_owned(),
            )));
        }
        let Some(next) = page.cursor() else {
            assert!(!page.truncated());
            break;
        };
        cursor = Some(MapCursor::from_token(&next.to_token()).unwrap());
    }
    assert_eq!(pages, 3);
    assert_eq!(declarations.len(), 3);
    assert!(edges.is_empty());
}

#[test]
fn semantic_source_is_authenticated_ranges_are_auditable_and_directions_hold() {
    let source = "fn source() { target(); }\n";
    let target = "fn target() {}\n";
    let fixture = Fixture::new(&[("source.rs", source), ("target.rs", target)]);
    let (workspace, index) = indexed(&fixture);
    let revision = index.revision();
    let source_id = id(&index, "source");
    let target_id = id(&index, "target");
    let facts = semantic_definition(
        &fixture,
        revision,
        "source.rs",
        source,
        "target.rs",
        target,
        PositionEncoding::Utf8,
    );
    let evidence = [SemanticRelationship::new(source_id, &facts[0])];
    let mut request = RepositoryMapRequest::default();
    request.expansion.relationships = vec![RelationshipKind::SemanticDefinition];

    request.expansion.seeds = vec![source_id];
    request.expansion.purpose = ExpansionPurpose::Dependencies;
    let dependencies = build_repository_map(
        &workspace,
        &index,
        &request,
        &evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    let dependencies_wire = wire(&dependencies);
    let edge = &dependencies_wire["edges"][0];
    assert_eq!(edge["source_declaration"], wire_id(source_id));
    assert_eq!(edge["target_declaration"], wire_id(target_id));
    assert_eq!(edge["fact_range"]["start_byte"], 3);
    assert_eq!(edge["fact_range"]["end_byte"], 9);
    assert_eq!(edge["semantic_target_range"], edge["fact_range"]);
    assert_eq!(edge["semantic_source_range"]["start_byte"], 0);
    assert_eq!(edge["semantic_source_range"]["end_byte"], 0);
    assert!(
        edge["source_range"]["start_byte"].as_u64().unwrap()
            < edge["source_range"]["end_byte"].as_u64().unwrap()
    );
    assert!(
        edge["target_range"]["start_byte"].as_u64().unwrap()
            < edge["target_range"]["end_byte"].as_u64().unwrap()
    );

    request.expansion.seeds = vec![target_id];
    request.expansion.purpose = ExpansionPurpose::Dependents;
    let dependents = build_repository_map(
        &workspace,
        &index,
        &request,
        &evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    assert!(
        wire(&dependents)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["declaration_id"] == wire_id(source_id))
    );

    request.expansion.purpose = ExpansionPurpose::Neighborhood;
    let neighborhood = build_repository_map(
        &workspace,
        &index,
        &request,
        &evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    assert!(
        wire(&neighborhood)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["declaration_id"] == wire_id(source_id))
    );

    let relabeled = [SemanticRelationship::new(target_id, &facts[0])];
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &relabeled,
            MapLimits::default(),
            None
        ),
        Err(MapError::InvalidFact(
            "semantic source declaration does not match origin URI"
        ))
    ));
}

#[test]
fn nested_same_file_location_authenticates_only_the_inner_source_declaration() {
    let source = "fn outer() {\nfn inner() { target(); }\n}\nfn target() {}\n";
    let fixture = Fixture::new(&[("lib.rs", source)]);
    let (workspace, index) = indexed(&fixture);
    let revision = index.revision();
    let outer_id = id(&index, "outer");
    let inner_id = id(&index, "outer::inner");
    let target_id = id(&index, "target");
    let snapshot = LspWorkspaceSnapshot::new(
        fixture.root.clone(),
        revision,
        1,
        vec![SnapshotFile::new(
            "lib.rs",
            source.as_bytes().to_vec(),
            false,
        )],
        vec![OpenDocument::new(
            fixture.uri("lib.rs"),
            DocumentVersion::new(1),
            source.to_owned(),
        )],
        server(),
        PositionEncoding::Utf8,
        EditLimits::default(),
        FactLimits::default(),
    )
    .unwrap();
    let accepted = accepted_definition(&fixture, revision, source, 1, 3, 3, 9);
    let facts = normalize_semantic_locations(&snapshot, &accepted).unwrap();
    let request = RepositoryMapRequest {
        expansion: ExpansionRequest {
            seeds: vec![inner_id],
            purpose: ExpansionPurpose::Dependencies,
            relationships: vec![RelationshipKind::SemanticDefinition],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };

    let inner = build_repository_map(
        &workspace,
        &index,
        &request,
        &[SemanticRelationship::new(inner_id, &facts[0])],
        MapLimits::default(),
        None,
    )
    .unwrap();
    assert!(
        wire(&inner)["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| {
                edge["source_declaration"] == wire_id(inner_id)
                    && edge["target_declaration"] == wire_id(target_id)
                    && edge["relationship"] == "semantic_definition"
            })
    );

    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[SemanticRelationship::new(outer_id, &facts[0])],
            MapLimits::default(),
            None,
        ),
        Err(MapError::InvalidFact(
            "semantic source declaration is not the smallest declaration containing request origin"
        ))
    ));
}

#[test]
fn duplicate_semantic_provenance_is_canonical_across_encoding_and_input_order() {
    let source = "fn source() { target(); }\n";
    let target = "fn target() {}\n";
    let fixture = Fixture::new(&[("source.rs", source), ("target.rs", target)]);
    let (workspace, index) = indexed(&fixture);
    let source_id = id(&index, "source");
    let utf8 = semantic_definition(
        &fixture,
        index.revision(),
        "source.rs",
        source,
        "target.rs",
        target,
        PositionEncoding::Utf8,
    );
    let utf16 = semantic_definition(
        &fixture,
        index.revision(),
        "source.rs",
        source,
        "target.rs",
        target,
        PositionEncoding::Utf16,
    );
    let request = RepositoryMapRequest {
        expansion: kit::workspace::map::ExpansionRequest {
            seeds: vec![source_id],
            purpose: ExpansionPurpose::Dependencies,
            relationships: vec![RelationshipKind::SemanticDefinition],
            ..ExpansionRequest::default()
        },
        ..RepositoryMapRequest::default()
    };
    let first_evidence = [
        SemanticRelationship::new(source_id, &utf16[0]),
        SemanticRelationship::new(source_id, &utf8[0]),
    ];
    let reversed_evidence = [first_evidence[1], first_evidence[0]];
    let first = build_repository_map(
        &workspace,
        &index,
        &request,
        &first_evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    let reversed = build_repository_map(
        &workspace,
        &index,
        &request,
        &reversed_evidence,
        MapLimits::default(),
        None,
    )
    .unwrap();
    assert_eq!(
        first.to_canonical_json().unwrap(),
        reversed.to_canonical_json().unwrap()
    );
    let first_wire = wire(&first);
    assert_eq!(first_wire["edges"].as_array().unwrap().len(), 1);
    assert_eq!(
        first_wire["edges"][0]["provenance"]["fact"]["position_encoding"],
        "utf-8"
    );
}

#[test]
fn degree_is_checked_only_for_reached_nodes_and_hostile_evidence_obeys_work_limit() {
    let fixture = Fixture::new(&[(
        "lib.rs",
        "mod busy { fn one() {} fn two() {} fn three() {} } fn isolated() {}\n",
    )]);
    let (workspace, index) = indexed(&fixture);
    let request = RepositoryMapRequest {
        expansion: kit::workspace::map::ExpansionRequest {
            seeds: vec![id(&index, "isolated")],
            purpose: ExpansionPurpose::Neighborhood,
            relationships: vec![RelationshipKind::Contains],
            ..ExpansionRequest::default()
        },
        budget: MapBudget {
            max_degree: 1,
            ..MapBudget::default()
        },
        ..RepositoryMapRequest::default()
    };
    let isolated = build_repository_map(
        &workspace,
        &index,
        &request,
        &[],
        MapLimits::default(),
        None,
    )
    .unwrap();
    assert_eq!(
        wire(&isolated)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| !entry["hops"].is_null())
            .count(),
        1
    );

    let source = "fn source() { target(); }\n";
    let target = "fn target() {}\n";
    let semantic_fixture = Fixture::new(&[("source.rs", source), ("target.rs", target)]);
    let (semantic_workspace, semantic_index) = indexed(&semantic_fixture);
    let source_id = id(&semantic_index, "source");
    let facts = semantic_definition(
        &semantic_fixture,
        semantic_index.revision(),
        "source.rs",
        source,
        "target.rs",
        target,
        PositionEncoding::Utf8,
    );
    let hostile = vec![SemanticRelationship::new(source_id, &facts[0]); 100_000];
    let limits = MapLimits {
        max_work: 2,
        ..MapLimits::default()
    };
    assert!(matches!(
        build_repository_map(
            &semantic_workspace,
            &semantic_index,
            &RepositoryMapRequest::default(),
            &hostile,
            limits,
            None
        ),
        Err(MapError::InvalidRequest("map work limit exceeded"))
    ));

    let evidence = [
        SemanticRelationship::new(source_id, &facts[0]),
        SemanticRelationship::new(source_id, &facts[0]),
    ];
    assert!(matches!(
        build_repository_map(
            &semantic_workspace,
            &semantic_index,
            &RepositoryMapRequest::default(),
            &evidence,
            MapLimits {
                max_semantic_relationships: 1,
                ..MapLimits::default()
            },
            None
        ),
        Err(MapError::InvalidRequest("too many semantic relationships"))
    ));
    assert!(matches!(
        build_repository_map(
            &semantic_workspace,
            &semantic_index,
            &RepositoryMapRequest::default(),
            &evidence[..1],
            MapLimits {
                max_input_bytes: 1,
                ..MapLimits::default()
            },
            None
        ),
        Err(MapError::InvalidRequest("input byte limit exceeded"))
    ));
}

#[test]
fn tiny_mandatory_output_budget_fails_before_large_payload_rendering() {
    let source = format!("fn enormous() {{ let _ = \"{}\"; }}\n", "x".repeat(8_000));
    let fixture = Fixture::new(&[("lib.rs", &source)]);
    let (workspace, index) = indexed(&fixture);
    let declaration = index.entries()[0].syntax_records[0].declaration_id();
    let request = RepositoryMapRequest {
        expansion: kit::workspace::map::ExpansionRequest {
            seeds: vec![DeclarationId::from(declaration)],
            ..kit::workspace::map::ExpansionRequest::default()
        },
        budget: MapBudget {
            max_estimated_tokens: MapLimits::default().max_estimated_tokens,
            max_result_bytes: 128,
            ..MapBudget::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &request,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::ResultBytes))
    ));
}

#[test]
fn hostile_escaped_source_is_precharged_and_tiny_graph_work_fails_deterministically() {
    let hostile = "\\\"\u{0008}".repeat(600);
    let source = format!("fn hostile() {{ let _ = {hostile:?}; }}\nfn neighbor() {{}}\n");
    let fixture = Fixture::new(&[("lib.rs", &source)]);
    let (workspace, index) = indexed(&fixture);
    let hostile_id = id(&index, "hostile");
    let mandatory = RepositoryMapRequest {
        expansion: ExpansionRequest {
            seeds: vec![hostile_id],
            relationships: Vec::new(),
            ..ExpansionRequest::default()
        },
        budget: MapBudget {
            max_estimated_tokens: MapLimits::default().max_estimated_tokens,
            max_result_bytes: 2_000,
            ..MapBudget::default()
        },
        ..RepositoryMapRequest::default()
    };
    assert!(matches!(
        build_repository_map(
            &workspace,
            &index,
            &mandatory,
            &[],
            MapLimits::default(),
            None
        ),
        Err(MapError::BoundExceeded(MapBound::ResultBytes))
    ));

    let tiny = MapLimits {
        max_work: 8,
        ..MapLimits::default()
    };
    for _ in 0..2 {
        assert!(matches!(
            build_repository_map(
                &workspace,
                &index,
                &RepositoryMapRequest::default(),
                &[],
                tiny,
                None
            ),
            Err(MapError::InvalidRequest("map work limit exceeded"))
        ));
    }
}
