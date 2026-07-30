#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

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
            FactLimits, LspWorkspaceSnapshot, OpenDocument, SemanticFact, SemanticRelationKind,
            SnapshotFile, normalize_live_diagnostics, normalize_semantic_locations,
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
        edit::ir::EditLimits,
        graph::structure::{
            CoverageStatus, EdgeKind, GraphBound, GraphError, GraphOptions, NodeKind, RangeKind,
            StructureGraph, StructureGraphProvider,
        },
        index::meta::{IndexOptions, MetadataIndex},
        map::{DeclarationId, SemanticRelationship},
        revision::{ManagedWorkspace, RevisionId, RevisionOptions},
    },
};
use serde_json::{Value, json};
use url::Url;

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-graph-structure-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).unwrap();
        Self {
            root,
            workspace_path,
        }
    }

    fn cargo_workspace() -> Self {
        let fixture = Self::new();
        fixture.write(
            "Cargo.toml",
            r#"[package]
name = "root"
version = "0.1.0"
edition = "2024"

[workspace]
members = ["crates/*"]
exclude = ["crates/excluded"]

[workspace.dependencies]
helper_alias = { package = "helper", path = "crates/helper" }

[dependencies]
helper_alias = { workspace = true }

[dev-dependencies]
helper_alias = { workspace = true }

[[bin]]
name = "tool"
path = "src/bin/tool.rs"

[[test]]
name = "integration"
path = "tests/integration.rs"
"#,
        );
        fixture.write(
            "src/lib.rs",
            r#"mod nested;

pub fn production() { let _spelling = "calls references #[test]"; }

#[test]
fn built_in_test() { production(); }

#[cfg(any())]
#[test]
fn false_cfg_test() {}

#[cfg(
    any()
)]
#[test]
fn multiline_false_cfg_test() {}

#[cfg(feature = "unknown")]
#[test]
fn unresolved_cfg_test() {}

#[tokio::test]
async fn ignored_tokio_test() {}

#[cfg_attr(test, test)]
fn ignored_cfg_attr_test() {}

#[cfg_attr(
    test,
    test
)]
fn multiline_cfg_attr_test() {}

#[some::test]
fn unresolved_macro_test() {}
"#,
        );
        fixture.write(
            "src/nested.rs",
            "pub struct Nested;\nimpl Nested { pub fn value(&self) -> u8 { 1 } }\n",
        );
        fixture.write("src/main.rs", "fn main() {}\n#[test]\nfn main_test() {}\n");
        fixture.write(
            "src/bin/tool.rs",
            "fn main() {}\n#[test]\nfn binary_test() {}\n",
        );
        fixture.write(
            "tests/integration.rs",
            "#[test]\nfn integration_case() { assert_eq!(2 + 2, 4); }\n",
        );
        fixture.write(
            "examples/demo.rs",
            "fn main() {}\n#[test]\nfn example_is_not_a_test() {}\n",
        );
        fixture.write("orphan.rs", "#[test]\nfn orphan_is_not_a_test() {}\n");
        fixture.write(
            "crates/helper/Cargo.toml",
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        fixture.write("crates/helper/src/lib.rs", "pub fn helper() -> u8 { 7 }\n");
        fixture.write("README.md", "real fixture\n");
        fixture
    }

    fn write(&self, path: &str, source: impl AsRef<[u8]>) {
        let path = self.workspace_path.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, source).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.workspace_path.join(path)).unwrap()
    }

    fn uri(&self, path: &str) -> String {
        Url::from_file_path(self.workspace_path.join(path))
            .unwrap()
            .to_string()
    }

    fn open(&self) -> ManagedWorkspace {
        ManagedWorkspace::open_with_options(
            &self.workspace_path,
            RevisionOptions {
                max_entries: 10_000,
                max_name_bytes: 8 * 1024 * 1024,
                max_bytes: 64 * 1024 * 1024,
                max_memory_bytes: 128 * 1024 * 1024,
                max_depth: 128,
                max_scan_time: Duration::from_secs(5),
                max_scan_attempts: 2,
                watcher_interval: Duration::from_millis(5),
                reconciliation_interval: Duration::from_secs(60),
                metadata_path: Some(self.root.join("revision.state")),
            },
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn build_index(workspace: &ManagedWorkspace) -> MetadataIndex {
    let revision = workspace.current_revision().unwrap().id();
    MetadataIndex::build(workspace, revision, &IndexOptions::default()).unwrap()
}

fn refresh<'a>(
    provider: &'a mut StructureGraphProvider,
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    options: &GraphOptions,
) -> &'a StructureGraph {
    provider
        .refresh(workspace, index, options, &[], &[])
        .unwrap()
}

fn graph_node<'a>(
    graph: &'a StructureGraph,
    kind: NodeKind,
    name: &str,
) -> &'a kit::workspace::graph::structure::GraphNode {
    graph
        .nodes()
        .iter()
        .find(|node| node.kind() == kind && node.name() == name)
        .unwrap_or_else(|| panic!("missing {kind:?} node {name}"))
}

fn has_test(graph: &StructureGraph, name: &str) -> bool {
    graph
        .nodes()
        .iter()
        .any(|node| node.kind() == NodeKind::Test && node.name() == name)
}

#[test]
fn cargo_and_tree_sitter_emit_only_reachable_exact_tests() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());

    graph_node(graph, NodeKind::Package, "root");
    graph_node(graph, NodeKind::Package, "helper");
    for name in [
        "built_in_test",
        "main_test",
        "binary_test",
        "integration_case",
        "integration",
        "multiline_cfg_attr_test",
    ] {
        assert!(has_test(graph, name), "missing exact test {name}");
    }
    for name in [
        "false_cfg_test",
        "multiline_false_cfg_test",
        "unresolved_cfg_test",
        "ignored_tokio_test",
        "example_is_not_a_test",
        "orphan_is_not_a_test",
        "unresolved_macro_test",
    ] {
        assert!(!has_test(graph, name), "false test node {name}");
    }
    assert!(has_test(graph, "ignored_cfg_attr_test"));
    assert!(graph.coverage().iter().any(|item| {
        item.relation() == EdgeKind::Tests
            && item.status() == CoverageStatus::Unavailable
            && item.detail() == "test cfg reachability is unresolved"
    }));
    assert!(graph.edges().iter().all(|edge| {
        edge.revision() == index.revision()
            && edge.provenance().revision() == index.revision()
            && edge.provenance().range().start_byte() <= edge.provenance().range().end_byte()
    }));
}

#[test]
fn explicit_targets_do_not_disable_independent_auto_targets() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        r#"[package]
name = "targets"
version = "0.1.0"
autobins = true
autoexamples = false
autotests = false
autobenches = false

[[bin]]
name = "tool"
path = "custom/tool.rs"

[[test]]
name = "disabled"
path = "tests/disabled.rs"
test = false

[[test]]
name = "no_harness"
path = "tests/no_harness.rs"
harness = false

[[test]]
name = "feature_gated"
path = "tests/feature.rs"
required-features = ["extra"]
"#,
    );
    fixture.write(
        "src/main.rs",
        "fn main() {}\n#[test]\nfn automatic_main() {}\n",
    );
    fixture.write(
        "custom/tool.rs",
        "fn main() {}\n#[test]\nfn explicit_tool() {}\n",
    );
    fixture.write(
        "src/bin/auto.rs",
        "fn main() {}\n#[test]\nfn automatic_bin() {}\n",
    );
    fixture.write("tests/disabled.rs", "#[test]\nfn disabled_case() {}\n");
    fixture.write("tests/no_harness.rs", "#[test]\nfn no_harness_case() {}\n");
    fixture.write("tests/feature.rs", "#[test]\nfn feature_case() {}\n");
    fixture.write("tests/ignored.rs", "#[test]\nfn ignored_auto_test() {}\n");
    fixture.write("examples/ignored.rs", "#[test]\nfn ignored_example() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());

    for name in ["automatic_main", "explicit_tool", "automatic_bin"] {
        assert!(has_test(graph, name));
    }
    for name in [
        "disabled_case",
        "no_harness_case",
        "feature_case",
        "ignored_auto_test",
        "ignored_example",
    ] {
        assert!(!has_test(graph, name));
    }
}

#[test]
fn directory_targets_are_inferred_and_dual_forms_are_ambiguous() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        r#"[package]
name = "directory-targets"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "tool"

[[test]]
name = "integration"

[[example]]
name = "demo"

[[bench]]
name = "perf"
"#,
    );
    fixture.write("src/main.rs", "fn main() {}\n#[test]\nfn main_case() {}\n");
    fixture.write(
        "src/bin/tool/main.rs",
        "fn main() {}\n#[test]\nfn tool_case() {}\n",
    );
    fixture.write(
        "tests/integration/main.rs",
        "#[test]\nfn integration_case() {}\n",
    );
    fixture.write("examples/demo/main.rs", "#[test]\nfn example_case() {}\n");
    fixture.write("benches/perf/main.rs", "#[test]\nfn bench_case() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    for name in ["main_case", "tool_case", "integration_case"] {
        assert!(
            has_test(graph, name),
            "missing directory target test {name}"
        );
    }
    for name in ["example_case", "bench_case"] {
        assert!(!has_test(graph, name), "false directory target test {name}");
    }

    fixture.write("src/bin/tool.rs", "fn main() {}\n");
    let ambiguous = build_index(&workspace);
    assert!(matches!(
        provider.refresh(
            &workspace,
            &ambiguous,
            &GraphOptions::default(),
            &[],
            &[]
        ),
        Err(GraphError::MalformedManifest { reason, .. }) if reason.contains("ambiguous")
    ));

    let missing_name = Fixture::new();
    missing_name.write(
        "Cargo.toml",
        "[package]\nname=\"missing\"\nversion=\"0.1.0\"\n[[bin]]\npath=\"src/main.rs\"\n",
    );
    missing_name.write("src/main.rs", "fn main() {}\n");
    let workspace = missing_name.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(matches!(
        provider.refresh(&workspace, &index, &GraphOptions::default(), &[], &[]),
        Err(GraphError::MalformedManifest { reason, .. })
            if reason.contains("requires name")
    ));
}

#[test]
fn inherited_cfg_and_exact_cargo_target_defaults_are_conservative() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        r#"[package]
name = "named"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "named"

[[bin]]
name = "tool"

[[example]]
name = "demo"

[[bench]]
name = "perf"
"#,
    );
    fixture.write(
        "src/lib.rs",
        r#"#[cfg_attr(test, test)]
fn cfg_attr_test() {}

#[cfg(any())]
mod disabled { #[test] fn disabled_child() {} }

#[cfg(feature = "unknown")]
mod unresolved { #[test] fn unresolved_child() {} }
"#,
    );
    fixture.write(
        "src/main.rs",
        "fn main() {}\n#[test]\nfn package_bin_test() {}\n",
    );
    fixture.write(
        "src/bin/tool.rs",
        "fn main() {}\n#[test]\nfn named_bin_test() {}\n",
    );
    fixture.write(
        "examples/demo.rs",
        "#[test]\nfn example_default_false() {}\n",
    );
    fixture.write("benches/perf.rs", "#[test]\nfn bench_default_false() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    for name in ["cfg_attr_test", "package_bin_test", "named_bin_test"] {
        assert!(has_test(graph, name), "missing test {name}");
    }
    for name in [
        "disabled_child",
        "unresolved_child",
        "example_default_false",
        "bench_default_false",
    ] {
        assert!(!has_test(graph, name), "false test {name}");
    }
    assert!(graph.coverage().iter().any(|record| {
        record.relation() == EdgeKind::Tests
            && record.status() == CoverageStatus::Unavailable
            && record.detail() == "test cfg reachability is unresolved"
    }));

    let crate_disabled = Fixture::new();
    crate_disabled.write(
        "Cargo.toml",
        "[package]\nname=\"disabled\"\nversion=\"0.1.0\"\n",
    );
    crate_disabled.write(
        "src/lib.rs",
        "#![cfg(any())]\n#[test]\nfn crate_disabled_test() {}\n",
    );
    let workspace = crate_disabled.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(!has_test(
        refresh(&mut provider, &workspace, &index, &GraphOptions::default()),
        "crate_disabled_test"
    ));
}

#[test]
fn cfg_depth_is_bounded_and_exact_libtest_attributes_remain_exact() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"cfg-bounds\"\nversion=\"0.1.0\"\n",
    );
    let exact_cfg = format!("{}test{}", "all(".repeat(64), ")".repeat(64));
    let hostile_cfg = format!("{}test{}", "not(".repeat(300), ")".repeat(300));
    fixture.write(
        "src/lib.rs",
        format!(
            r#"#[cfg({exact_cfg})]
#[test]
fn nested_exact() {{}}

#[ignore = "slow test"]
#[test]
fn ignored_exact() {{}}

#[should_panic(expected = "boom")]
#[test]
fn panic_exact() {{ panic!("boom") }}

#[unknown]
#[test]
fn unknown_unavailable() {{}}

#[cfg({hostile_cfg})]
#[test]
fn hostile_unavailable() {{}}
"#,
        ),
    );
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());

    for name in ["nested_exact", "ignored_exact", "panic_exact"] {
        assert!(has_test(graph, name), "missing exact test {name}");
    }
    for name in ["unknown_unavailable", "hostile_unavailable"] {
        assert!(!has_test(graph, name), "false exact test {name}");
    }
    assert!(graph.coverage().iter().any(|record| {
        record.relation() == EdgeKind::Tests
            && record.status() == CoverageStatus::Unavailable
            && record.detail() == "test cfg reachability is unresolved"
    }));
}

#[test]
fn rust_attribute_staging_is_reserved_before_attribute_vectors() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"attribute-staging\"\nversion=\"0.1.0\"\n",
    );
    fixture.write(
        "src/lib.rs",
        format!("{}fn item() {{}}\n", "#[allow(dead_code)]\n".repeat(256)),
    );
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(matches!(
        provider.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: 8 * 1024,
                ..GraphOptions::default()
            },
            &[],
            &[]
        ),
        Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
    ));
    assert!(provider.graph().is_none());
}

#[test]
fn rust_record_maps_are_reserved_even_without_attributes() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"record-staging\"\nversion=\"0.1.0\"\n",
    );
    fixture.write(
        "src/lib.rs",
        (0..200)
            .map(|index| format!("fn item_{index}() {{}}\n"))
            .collect::<String>(),
    );
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let small_index = MetadataIndex::build(
        &workspace,
        revision,
        &IndexOptions {
            max_symbols_per_file: 1,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    let mut small = StructureGraphProvider::new();
    refresh(
        &mut small,
        &workspace,
        &small_index,
        &GraphOptions::default(),
    );
    let before_record_maps = small.metrics().peak_staging_bytes();

    let full_index = MetadataIndex::build(
        &workspace,
        revision,
        &IndexOptions {
            max_symbols_per_file: 256,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    let mut full = StructureGraphProvider::new();
    assert!(matches!(
        full.refresh(
            &workspace,
            &full_index,
            &GraphOptions {
                max_staging_bytes: before_record_maps,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
    ));
    assert!(full.graph().is_none());
}

#[test]
fn rust_cache_identity_separates_truncated_and_complete_syntax() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"cache-identity\"\nversion=\"0.1.0\"\n",
    );
    fixture.write(
        "src/lib.rs",
        "fn first() {}\n#[test]\nfn retained_only_when_complete() {}\n",
    );
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let truncated = MetadataIndex::build(
        &workspace,
        revision,
        &IndexOptions {
            max_symbols_per_file: 1,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    let mut incremental = StructureGraphProvider::new();
    assert!(!has_test(
        refresh(
            &mut incremental,
            &workspace,
            &truncated,
            &GraphOptions::default(),
        ),
        "retained_only_when_complete"
    ));

    let complete = MetadataIndex::build(&workspace, revision, &IndexOptions::default()).unwrap();
    let refreshed = refresh(
        &mut incremental,
        &workspace,
        &complete,
        &GraphOptions::default(),
    )
    .clone();
    assert!(has_test(&refreshed, "retained_only_when_complete"));
    let mut clean = StructureGraphProvider::new();
    assert_eq!(
        &refreshed,
        refresh(&mut clean, &workspace, &complete, &GraphOptions::default(),)
    );
}

#[test]
fn warm_rust_cache_identity_hashing_obeys_the_exact_work_budget() {
    let fixture = Fixture::new();
    fixture.write(
        "src/lib.rs",
        (0..200)
            .map(|index| format!("fn item_{index}() {{}}\n"))
            .collect::<String>(),
    );
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut warm = StructureGraphProvider::new();
    refresh(&mut warm, &workspace, &index, &GraphOptions::default());

    let mut measured = warm.clone();
    refresh(&mut measured, &workspace, &index, &GraphOptions::default());
    assert_eq!(measured.metrics().reused_subgraphs(), 1);
    let work = measured.metrics().consumed_work();

    let mut exact = warm.clone();
    exact
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: work,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(exact.metrics().consumed_work(), work);

    assert!(matches!(
        warm.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: work - 1,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::Work))
    ));
}

#[test]
fn package_named_bin_default_requires_exactly_one_cargo_candidate() {
    for candidate in ["src/main.rs", "src/bin/named.rs", "src/bin/named/main.rs"] {
        let fixture = Fixture::new();
        fixture.write(
            "Cargo.toml",
            "[package]\nname=\"named\"\nversion=\"0.1.0\"\nautobins=false\n[[bin]]\nname=\"named\"\n",
        );
        fixture.write(candidate, "fn main() {}\n#[test]\nfn candidate_test() {}\n");
        let workspace = fixture.open();
        let index = build_index(&workspace);
        let mut provider = StructureGraphProvider::new();
        assert!(has_test(
            refresh(&mut provider, &workspace, &index, &GraphOptions::default()),
            "candidate_test"
        ));
    }

    let ambiguous = Fixture::new();
    ambiguous.write(
        "Cargo.toml",
        "[package]\nname=\"named\"\nversion=\"0.1.0\"\nautobins=false\n[[bin]]\nname=\"named\"\n",
    );
    ambiguous.write("src/main.rs", "fn main() {}\n");
    ambiguous.write("src/bin/named.rs", "fn main() {}\n");
    let workspace = ambiguous.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(matches!(
        provider.refresh(&workspace, &index, &GraphOptions::default(), &[], &[]),
        Err(GraphError::MalformedManifest { reason, .. }) if reason.contains("ambiguous")
    ));

    let missing = Fixture::new();
    missing.write(
        "Cargo.toml",
        "[package]\nname=\"named\"\nversion=\"0.1.0\"\nautobins=false\n[[bin]]\nname=\"named\"\n",
    );
    let workspace = missing.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(matches!(
        provider.refresh(&workspace, &index, &GraphOptions::default(), &[], &[]),
        Err(GraphError::MalformedManifest { reason, .. })
            if reason.contains("no default source file")
    ));
}

#[test]
fn cargo_2015_explicit_targets_disable_unspecified_auto_discovery() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"legacy\"\nversion=\"0.1.0\"\nedition=\"2015\"\n[[bin]]\nname=\"tool\"\npath=\"tool.rs\"\n",
    );
    fixture.write("src/lib.rs", "#[test]\nfn legacy_auto_lib() {}\n");
    fixture.write("tool.rs", "fn main() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(!has_test(
        refresh(&mut provider, &workspace, &index, &GraphOptions::default()),
        "legacy_auto_lib"
    ));

    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"legacy\"\nversion=\"0.1.0\"\nedition=\"2015\"\nautolib=true\n[[bin]]\nname=\"tool\"\npath=\"tool.rs\"\n",
    );
    let index = build_index(&workspace);
    assert!(has_test(
        refresh(&mut provider, &workspace, &index, &GraphOptions::default()),
        "legacy_auto_lib"
    ));
}

#[test]
fn workspace_path_dependencies_close_membership_and_overrides_mark_imports_unavailable() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        r#"[workspace]
members = ["member"]
exclude = ["excluded"]

[workspace.dependencies]
shared = { path = "shared" }

[patch.crates-io]
patched = { path = "patched" }
"#,
    );
    fixture.write(
        "member/Cargo.toml",
        "[package]\nname=\"member\"\nversion=\"0.1.0\"\n[dependencies]\nbridge={path=\"../bridge\"}\n",
    );
    fixture.write("member/src/lib.rs", "pub fn member() {}\n");
    fixture.write(
        "bridge/Cargo.toml",
        "[package]\nname=\"bridge\"\nversion=\"0.1.0\"\n[dependencies]\nshared={workspace=true}\n",
    );
    fixture.write("bridge/src/lib.rs", "pub fn bridge() {}\n");
    fixture.write(
        "shared/Cargo.toml",
        "[package]\nname=\"shared\"\nversion=\"0.1.0\"\n",
    );
    fixture.write("shared/src/lib.rs", "pub fn shared() {}\n");
    fixture.write(
        "patched/Cargo.toml",
        "[package]\nname=\"patched\"\nversion=\"0.1.0\"\n",
    );
    fixture.write("patched/src/lib.rs", "pub fn patched() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let bridge = graph_node(graph, NodeKind::Package, "bridge").id();
    let shared = graph_node(graph, NodeKind::Package, "shared").id();
    assert!(graph.edges().iter().any(|edge| {
        edge.source() == bridge && edge.target() == shared && edge.kind() == EdgeKind::Imports
    }));
    assert!(graph.coverage().iter().any(|record| {
        record.subject() == Some(bridge)
            && record.relation() == EdgeKind::Imports
            && record.status() == CoverageStatus::Unavailable
            && record.detail() == "Cargo patch or replace resolution is unavailable"
    }));
}

#[test]
fn deep_workspace_dependency_closure_charges_each_dependency_on_every_pass() {
    let fixture = Fixture::new();
    fixture.write("Cargo.toml", "[workspace]\nmembers=[\"p0\"]\n");
    for index in 0..12 {
        let dependency = if index == 11 {
            String::new()
        } else {
            format!(
                "[dependencies]\np{}={{path=\"../p{}\"}}\n",
                index + 1,
                index + 1
            )
        };
        fixture.write(
            &format!("p{index}/Cargo.toml"),
            format!("[package]\nname=\"p{index}\"\nversion=\"0.1.0\"\n{dependency}"),
        );
        fixture.write(
            &format!("p{index}/src/lib.rs"),
            format!("pub fn p{index}() {{}}\n"),
        );
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let p10 = graph_node(graph, NodeKind::Package, "p10").id();
    let p11 = graph_node(graph, NodeKind::Package, "p11").id();
    assert!(graph.edges().iter().any(|edge| {
        edge.source() == p10 && edge.target() == p11 && edge.kind() == EdgeKind::Imports
    }));
    let consumed = provider.metrics().consumed_work();
    let mut below = StructureGraphProvider::new();
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: consumed - 1,
                ..GraphOptions::default()
            },
            &[],
            &[]
        ),
        Err(GraphError::BoundExceeded(GraphBound::Work))
    ));
}

#[test]
fn each_target_charges_every_compiled_syntax_record_inspected() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        r#"[package]
name="target-work"
version="0.1.0"
autobins=false

[[bin]]
name="one"
path="src/one.rs"

[[bin]]
name="two"
path="src/two.rs"
"#,
    );
    let declarations = (0..80)
        .map(|index| format!("fn item_{index}() {{}}\n"))
        .collect::<String>();
    fixture.write("src/one.rs", format!("fn main() {{}}\n{declarations}"));
    fixture.write("src/two.rs", format!("fn main() {{}}\n{declarations}"));
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut baseline = StructureGraphProvider::new();
    refresh(&mut baseline, &workspace, &index, &GraphOptions::default());
    let work = baseline.metrics().consumed_work();

    let mut exact = StructureGraphProvider::new();
    exact
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: work,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    let mut below = StructureGraphProvider::new();
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: work - 1,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::Work))
    ));
}

#[test]
fn workspace_glob_match_work_is_precharged_and_character_classes_are_valid() {
    let fixture = Fixture::new();
    let members = (0..20)
        .map(|index| format!("\"crates/p{index:02}[ab]\""))
        .collect::<Vec<_>>()
        .join(",");
    fixture.write("Cargo.toml", format!("[workspace]\nmembers=[{members}]\n"));
    for index in 0..20 {
        for suffix in ['a', 'b'] {
            fixture.write(
                &format!("crates/p{index:02}{suffix}/Cargo.toml"),
                format!("[package]\nname=\"p{index:02}{suffix}\"\nversion=\"0.1.0\"\n"),
            );
            fixture.write(
                &format!("crates/p{index:02}{suffix}/src/lib.rs"),
                "pub fn item() {}\n",
            );
        }
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(matches!(
        provider.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: 1_000,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::Work))
    ));
}

#[test]
fn workspace_globs_excludes_and_inherited_renamed_path_dependencies_are_exact() {
    let fixture = Fixture::cargo_workspace();
    fixture.write(
        "crates/excluded/Cargo.toml",
        "[package]\nname = \"excluded\"\nversion = \"0.1.0\"\n",
    );
    fixture.write("crates/excluded/src/lib.rs", "pub fn excluded() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let root = graph_node(graph, NodeKind::Package, "root").id();
    let helper = graph_node(graph, NodeKind::Package, "helper").id();
    assert!(graph.edges().iter().any(|edge| {
        edge.source() == root && edge.target() == helper && edge.kind() == EdgeKind::Imports
    }));
    assert!(graph_node(graph, NodeKind::Package, "excluded").id() != helper);
}

#[test]
fn missing_workspace_inheritance_is_explicitly_unavailable() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers=[\"member\"]\n[workspace.dependencies]\nknown={path=\"member\"}\n",
    );
    fixture.write(
        "member/Cargo.toml",
        "[package]\nname=\"member\"\nversion=\"0.1.0\"\n[dependencies]\nmissing={workspace=true}\n",
    );
    fixture.write("member/src/lib.rs", "pub fn member() {}\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let member = graph_node(graph, NodeKind::Package, "member").id();
    assert!(graph.coverage().iter().any(|item| {
        item.subject() == Some(member)
            && item.relation() == EdgeKind::Imports
            && item.status() == CoverageStatus::Unavailable
            && item.detail() == "workspace dependency inheritance is missing"
    }));
}

#[test]
fn one_file_edit_changes_only_affected_digests_and_matches_clean_rebuild() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let first_index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    refresh(
        &mut provider,
        &workspace,
        &first_index,
        &GraphOptions::default(),
    );
    let old_changed =
        graph_node(provider.graph().unwrap(), NodeKind::File, "src/nested.rs").structural_digest();
    let old_changed_subgraph =
        graph_node(provider.graph().unwrap(), NodeKind::File, "src/nested.rs").subgraph_digest();
    let helper_digest = graph_node(
        provider.graph().unwrap(),
        NodeKind::File,
        "crates/helper/src/lib.rs",
    )
    .subgraph_digest();

    fixture.write(
        "src/nested.rs",
        "pub struct Nested;\nimpl Nested { pub fn value(&self) -> u8 { 2 } }\n",
    );
    let second_index = build_index(&workspace);
    let incremental = refresh(
        &mut provider,
        &workspace,
        &second_index,
        &GraphOptions::default(),
    )
    .clone();
    assert_eq!(provider.metrics().parsed_fragments(), 0);
    assert_eq!(provider.metrics().reused_fragments(), 2);
    assert_eq!(provider.metrics().rebuilt_subgraphs(), 1);
    assert!(provider.metrics().reused_subgraphs() > 2);
    assert_eq!(
        provider.metrics().changed_paths()[0].as_str(),
        "src/nested.rs"
    );
    assert_ne!(
        graph_node(&incremental, NodeKind::File, "src/nested.rs").structural_digest(),
        old_changed
    );
    assert_ne!(
        graph_node(&incremental, NodeKind::File, "src/nested.rs").subgraph_digest(),
        old_changed_subgraph
    );
    assert_eq!(
        graph_node(&incremental, NodeKind::File, "crates/helper/src/lib.rs").subgraph_digest(),
        helper_digest
    );
    let mut clean = StructureGraphProvider::new();
    let rebuilt = refresh(
        &mut clean,
        &workspace,
        &second_index,
        &GraphOptions::default(),
    );
    assert_eq!(&incremental, rebuilt);
}

#[test]
fn package_nodes_expose_manifest_paths_not_internal_roots() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    assert_eq!(
        graph_node(graph, NodeKind::Package, "root")
            .path()
            .unwrap()
            .as_str(),
        "Cargo.toml"
    );
    assert_eq!(
        graph_node(graph, NodeKind::Package, "helper")
            .path()
            .unwrap()
            .as_str(),
        "crates/helper/Cargo.toml"
    );
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

fn accepted_notification(
    uri: &str,
    revision: kit::workspace::revision::RevisionId,
    text: &str,
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
                position_encoding: PositionEncoding::Utf8,
                execution_profile: profile(),
            },
            revision,
        )
        .unwrap();
    manager
        .open_document(
            service,
            uri.to_owned(),
            DocumentVersion::new(1),
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
        panic!("notification was not accepted");
    };
    manager.shutdown().unwrap();
    accepted
}

fn accepted_semantic_response(
    fixture: &Fixture,
    revision: RevisionId,
    source: &str,
    encoding: PositionEncoding,
    method: &str,
) -> AcceptedResponse {
    let uri = fixture.uri("src/lib.rs");
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
            method,
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":1,"character":3}
            }),
            manager.now_tick() + 10_000,
        )
        .unwrap();
    let frame = LspCodec::encode(
        &json!({
            "jsonrpc":"2.0",
            "id":token.request_id.get(),
            "result":{
                "uri":fixture.uri("src/lib.rs"),
                "range":{
                    "start":{"line":3,"character":3},
                    "end":{"line":3,"character":9}
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
        panic!("semantic response was not accepted");
    };
    manager.shutdown().unwrap();
    accepted
}

fn semantic_fact(
    fixture: &Fixture,
    index: &MetadataIndex,
    source: &str,
    encoding: PositionEncoding,
) -> Vec<SemanticFact> {
    semantic_fact_for(fixture, index, source, encoding, "textDocument/definition")
}

fn semantic_fact_for(
    fixture: &Fixture,
    index: &MetadataIndex,
    source: &str,
    encoding: PositionEncoding,
    method: &str,
) -> Vec<SemanticFact> {
    let uri = fixture.uri("src/lib.rs");
    let snapshot = LspWorkspaceSnapshot::new(
        fixture.workspace_path.clone(),
        index.revision(),
        1,
        vec![SnapshotFile::new(
            "src/lib.rs",
            source.as_bytes().to_vec(),
            false,
        )],
        vec![OpenDocument::new(
            uri,
            DocumentVersion::new(1),
            source.to_owned(),
        )],
        server(),
        encoding,
        EditLimits::default(),
        FactLimits::default(),
    )
    .unwrap();
    normalize_semantic_locations(
        &snapshot,
        &accepted_semantic_response(fixture, index.revision(), source, encoding, method),
    )
    .unwrap()
}

#[test]
fn semantic_edges_reuse_validated_nested_resolution_and_retain_full_provenance() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"semantic\"\nversion=\"0.1.0\"\n",
    );
    let source = "fn outer() {\nfn inner() { target(); }\n}\nfn target() {}\n";
    fixture.write("src/lib.rs", source);
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let inner = index
        .entries()
        .iter()
        .flat_map(|entry| entry.syntax_records.iter())
        .find(|record| record.qualified_name().value().as_str() == "outer::inner")
        .unwrap();
    let outer = index
        .entries()
        .iter()
        .flat_map(|entry| entry.syntax_records.iter())
        .find(|record| record.qualified_name().value().as_str() == "outer")
        .unwrap();
    let utf8 = semantic_fact(&fixture, &index, source, PositionEncoding::Utf8);
    let utf16 = semantic_fact(&fixture, &index, source, PositionEncoding::Utf16);
    let evidence = [
        SemanticRelationship::new(DeclarationId::from(inner.declaration_id()), &utf8[0]),
        SemanticRelationship::new(DeclarationId::from(inner.declaration_id()), &utf16[0]),
    ];
    let mut first = StructureGraphProvider::new();
    let first = first
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &evidence)
        .unwrap()
        .clone();
    let mut second = StructureGraphProvider::new();
    let second = second
        .refresh(
            &workspace,
            &index,
            &GraphOptions::default(),
            &[],
            &[evidence[1], evidence[0]],
        )
        .unwrap();
    assert_eq!(&first, second);
    let edges = first
        .edges()
        .iter()
        .filter(|edge| {
            edge.kind() == EdgeKind::References && edge.provenance().semantic().is_some()
        })
        .collect::<Vec<_>>();
    assert_eq!(edges.len(), 2);
    assert_eq!(
        edges
            .iter()
            .map(|edge| edge.provenance().evidence_digest())
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );
    for edge in edges {
        assert_eq!(edge.provenance().confidence_millis(), 1_000);
        let provenance = edge.provenance().semantic().unwrap();
        assert_eq!(provenance.relation(), SemanticRelationKind::Definition);
        assert_eq!(provenance.document_version(), 1);
        assert_eq!(provenance.origin_path().as_str(), "src/lib.rs");
        assert!(provenance.origin_uri().starts_with("file://"));
        assert!(provenance.request_generation() > 0);
        assert!(provenance.request_id() > 0);
        assert_eq!(provenance.fact_range().start_line(), 4);
        assert_eq!(provenance.target_range(), provenance.fact_range());
        assert!(!provenance.server_artifact().is_empty());
        assert!(!provenance.server_configuration().is_empty());
    }
    assert!(first.coverage().iter().any(|record| {
        record.subject().is_none()
            && record.relation() == EdgeKind::References
            && record.status() == CoverageStatus::ObservedPartial
    }));

    let mut measured = StructureGraphProvider::new();
    measured
        .refresh(&workspace, &index, &GraphOptions::default(), &[], &evidence)
        .unwrap();
    let consumed_work = measured.metrics().consumed_work();
    let mut exact = StructureGraphProvider::new();
    exact
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: consumed_work,
                ..GraphOptions::default()
            },
            &[],
            &evidence,
        )
        .unwrap();
    assert_eq!(exact.metrics().consumed_work(), consumed_work);
    let mut below = StructureGraphProvider::new();
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: consumed_work - 1,
                ..GraphOptions::default()
            },
            &[],
            &evidence,
        ),
        Err(GraphError::BoundExceeded(GraphBound::Work))
    ));

    let mut rejected = StructureGraphProvider::new();
    assert!(matches!(
        rejected.refresh(
            &workspace,
            &index,
            &GraphOptions::default(),
            &[],
            &[SemanticRelationship::new(
                DeclarationId::from(outer.declaration_id()),
                &utf8[0],
            )],
        ),
        Err(GraphError::InvalidEvidence(
            "semantic source declaration is not the smallest declaration containing request origin"
        ))
    ));
}

#[test]
fn semantic_graph_edges_keep_relation_specific_factual_direction() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"semantic-directions\"\nversion=\"0.1.0\"\n",
    );
    let source = "fn source() {\n target();\n}\nfn target() {}\n";
    fixture.write("src/lib.rs", source);
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let source_id = index
        .entries()
        .iter()
        .flat_map(|entry| entry.syntax_records.iter())
        .find(|record| record.qualified_name().value().as_str() == "source")
        .unwrap()
        .declaration_id();
    let target_id = index
        .entries()
        .iter()
        .flat_map(|entry| entry.syntax_records.iter())
        .find(|record| record.qualified_name().value().as_str() == "target")
        .unwrap()
        .declaration_id();
    for (method, relation, inverse) in [
        (
            "textDocument/declaration",
            SemanticRelationKind::Declaration,
            false,
        ),
        (
            "textDocument/definition",
            SemanticRelationKind::Definition,
            false,
        ),
        (
            "textDocument/typeDefinition",
            SemanticRelationKind::TypeDefinition,
            false,
        ),
        (
            "textDocument/references",
            SemanticRelationKind::Reference,
            true,
        ),
        (
            "textDocument/implementation",
            SemanticRelationKind::Implementation,
            true,
        ),
    ] {
        let facts = semantic_fact_for(&fixture, &index, source, PositionEncoding::Utf8, method);
        let evidence = [SemanticRelationship::new(
            DeclarationId::from(source_id),
            &facts[0],
        )];
        let mut provider = StructureGraphProvider::new();
        let graph = provider
            .refresh(&workspace, &index, &GraphOptions::default(), &[], &evidence)
            .unwrap();
        let edge = graph
            .edges()
            .iter()
            .find(|edge| {
                edge.provenance()
                    .semantic()
                    .is_some_and(|semantic| semantic.relation() == relation)
            })
            .unwrap();
        assert_eq!(
            (edge.source().as_bytes(), edge.target().as_bytes()),
            if inverse {
                (target_id, source_id)
            } else {
                (source_id, target_id)
            }
        );
    }
}

fn diagnostics(
    fixture: &Fixture,
    index: &MetadataIndex,
    records: Vec<Value>,
) -> Vec<kit::verify::lsp::facts::LiveDiagnostic> {
    let path = "src/lib.rs";
    let text = fixture.read(path);
    let uri = fixture.uri(path);
    let snapshot = LspWorkspaceSnapshot::new(
        fixture.workspace_path.clone(),
        index.revision(),
        1,
        vec![SnapshotFile::new(path, text.as_bytes().to_vec(), false)],
        vec![OpenDocument::new(
            uri.clone(),
            DocumentVersion::new(1),
            text.clone(),
        )],
        server(),
        PositionEncoding::Utf8,
        EditLimits::default(),
        FactLimits::default(),
    )
    .unwrap();
    let accepted = accepted_notification(
        &uri,
        index.revision(),
        &text,
        json!({"uri":uri,"version":1,"diagnostics":records}),
    );
    normalize_live_diagnostics(&snapshot, &accepted).unwrap()
}

#[test]
fn real_distinct_diagnostics_are_length_framed_canonical_and_co_located() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let first = json!({
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "severity":2,"code":"ab","source":"c","message":"first"
    });
    let second = json!({
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "severity":2,"code":"a","source":"bc","message":"second"
    });
    let ordered = diagnostics(&fixture, &index, vec![first.clone(), second.clone()]);
    let reversed = diagnostics(&fixture, &index, vec![second, first]);
    assert_ne!(ordered[0], ordered[1]);

    let mut left = StructureGraphProvider::new();
    let left = left
        .refresh(&workspace, &index, &GraphOptions::default(), &ordered, &[])
        .unwrap()
        .clone();
    let mut right = StructureGraphProvider::new();
    let right = right
        .refresh(&workspace, &index, &GraphOptions::default(), &reversed, &[])
        .unwrap();
    assert_eq!(&left, right);
    let diagnostic_nodes = left
        .nodes()
        .iter()
        .filter(|node| node.kind() == NodeKind::Diagnostic)
        .collect::<Vec<_>>();
    assert_eq!(diagnostic_nodes.len(), 2);
    for node in diagnostic_nodes {
        let edge = left
            .edges()
            .iter()
            .find(|edge| edge.target() == node.id())
            .unwrap();
        assert_eq!(node.path(), edge.provenance().path());
        assert_eq!(node.range(), Some(edge.provenance().range()));
        assert_eq!(edge.provenance().range_kind(), RangeKind::NormalizedFact);
    }
}

#[test]
fn crlf_and_utf8_ranges_use_exact_byte_and_line_boundaries() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"ranges\"\nversion=\"0.1.0\"\n",
    );
    fixture.write("src/lib.rs", "fn café() {}\r\nfn next() {}\r\n");
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let records = diagnostics(
        &fixture,
        &index,
        vec![json!({
            "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":8}},
            "severity":1,"message":"utf8 range"
        })],
    );
    let mut provider = StructureGraphProvider::new();
    let graph = provider
        .refresh(&workspace, &index, &GraphOptions::default(), &records, &[])
        .unwrap();
    let diagnostic = graph_node(graph, NodeKind::Diagnostic, "utf8 range");
    let range = diagnostic.range().unwrap();
    assert_eq!((range.start_byte(), range.end_byte()), (3, 8));
    assert_eq!((range.start_line(), range.end_line()), (1, 1));
    let file = graph_node(graph, NodeKind::File, "src/lib.rs");
    assert_eq!(file.range().unwrap().end_line(), 3);
}

#[test]
fn malformed_and_escaping_manifests_fail_atomically() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let graph = provider.graph().unwrap().clone();
    let metrics = provider.metrics().clone();
    let cache = provider.cache_usage();

    fixture.write("Cargo.toml", "[package\nname = \"broken\"\n");
    let malformed_index = build_index(&workspace);
    assert!(matches!(
        provider.refresh(
            &workspace,
            &malformed_index,
            &GraphOptions::default(),
            &[],
            &[]
        ),
        Err(GraphError::MalformedManifest { .. })
    ));
    assert_eq!(provider.graph(), Some(&graph));
    assert_eq!(provider.metrics(), &metrics);
    assert_eq!(provider.cache_usage(), cache);

    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"root\"\nversion=\"0.1.0\"\n[dependencies]\nbad={path=\"../../../outside\"}\n",
    );
    let escaping_index = build_index(&workspace);
    assert!(matches!(
        provider.refresh(
            &workspace,
            &escaping_index,
            &GraphOptions::default(),
            &[],
            &[]
        ),
        Err(GraphError::UnsafePath(_)) | Err(GraphError::MissingPathDependency { .. })
    ));
    assert_eq!(provider.graph(), Some(&graph));
    assert_eq!(provider.cache_usage(), cache);
}

#[test]
fn toml_preflight_and_cardinality_bounds_fail_before_publication() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    for (options, bound) in [
        (
            GraphOptions {
                max_manifest_input_bytes: 32,
                ..GraphOptions::default()
            },
            GraphBound::ManifestInputBytes,
        ),
        (
            GraphOptions {
                max_toml_items: 1,
                ..GraphOptions::default()
            },
            GraphBound::TomlItems,
        ),
        (
            GraphOptions {
                max_targets_per_manifest: 1,
                ..GraphOptions::default()
            },
            GraphBound::Targets,
        ),
        (
            GraphOptions {
                max_member_patterns: 1,
                ..GraphOptions::default()
            },
            GraphBound::MemberPatterns,
        ),
    ] {
        let mut provider = StructureGraphProvider::new();
        assert!(matches!(
            provider.refresh(&workspace, &index, &options, &[], &[]),
            Err(GraphError::BoundExceeded(actual)) if actual == bound
        ));
        assert!(provider.graph().is_none());
        assert_eq!(provider.cache_usage().entries(), 0);
    }
}

#[test]
fn toml_dotted_depth_and_array_table_cardinality_fail_in_preflight() {
    for (source, options, expected) in [
        (
            "[[bin]]\n[[bin]]\n",
            GraphOptions {
                max_targets_per_manifest: 1,
                ..GraphOptions::default()
            },
            GraphBound::Targets,
        ),
        (
            "[[dependencies.one]]\n[[dependencies.two]]\n",
            GraphOptions {
                max_dependencies_per_manifest: 1,
                ..GraphOptions::default()
            },
            GraphBound::Dependencies,
        ),
        (
            "[[workspace.dependencies.one]]\n[[workspace.dependencies.two]]\n",
            GraphOptions {
                max_workspace_dependencies: 1,
                ..GraphOptions::default()
            },
            GraphBound::WorkspaceDependencies,
        ),
        (
            "a.b.c.d = 1\n",
            GraphOptions {
                max_toml_nesting: 3,
                ..GraphOptions::default()
            },
            GraphBound::TomlNesting,
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write("Cargo.toml", source);
        let workspace = fixture.open();
        let index = build_index(&workspace);
        let mut provider = StructureGraphProvider::new();
        assert!(matches!(
            provider.refresh(&workspace, &index, &options, &[], &[]),
            Err(GraphError::BoundExceeded(actual)) if actual == expected
        ));
        assert!(provider.graph().is_none());
        assert_eq!(provider.cache_usage().entries(), 0);
    }
}

#[test]
fn toml_preflight_counts_dependency_table_key_assignments_before_parse() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        r#"[dependencies]
a="1"
[dev-dependencies]
b="1"
[build-dependencies]
c="1"
[target.'cfg(unix)'.dependencies]
d="1"
[target.'cfg(unix)'.dev-dependencies]
e="1"
[target.'cfg(unix)'.build-dependencies]
f="1"
[workspace.dependencies]
g="1"
h="1"
[package]
broken = [
"#,
    );
    let workspace = fixture.open();
    let index = build_index(&workspace);

    let mut exact = StructureGraphProvider::new();
    assert!(matches!(
        exact.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_dependencies_per_manifest: 6,
                max_workspace_dependencies: 2,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::MalformedManifest { .. })
    ));
    let mut dependency_below = StructureGraphProvider::new();
    assert!(matches!(
        dependency_below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_dependencies_per_manifest: 5,
                max_workspace_dependencies: 2,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::Dependencies))
    ));
    let mut workspace_below = StructureGraphProvider::new();
    assert!(matches!(
        workspace_below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_dependencies_per_manifest: 6,
                max_workspace_dependencies: 1,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::WorkspaceDependencies))
    ));
}

#[test]
fn line_index_and_combined_staging_peaks_are_preflighted_exactly() {
    let hostile = Fixture::new();
    hostile.write(
        "Cargo.toml",
        "[package]\nname=\"lines\"\nversion=\"0.1.0\"\n",
    );
    hostile.write("src/lib.rs", "\n".repeat(20_000));
    let workspace = hostile.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    assert!(matches!(
        provider.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: 4_096,
                ..GraphOptions::default()
            },
            &[],
            &[]
        ),
        Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
    ));

    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut baseline = StructureGraphProvider::new();
    refresh(&mut baseline, &workspace, &index, &GraphOptions::default());
    let peak = baseline.metrics().peak_staging_bytes();
    assert!(peak > 1);
    let mut exact = StructureGraphProvider::new();
    exact
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: peak,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(exact.metrics().peak_staging_bytes(), peak);
    let mut below = StructureGraphProvider::new();
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: peak - 1,
                ..GraphOptions::default()
            },
            &[],
            &[]
        ),
        Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
    ));
}

#[test]
fn replacing_a_published_graph_retains_its_logical_bytes_until_commit() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut measured = StructureGraphProvider::new();
    refresh(&mut measured, &workspace, &index, &GraphOptions::default());
    assert!(measured.graph().unwrap().logical_bytes() > 0);
    refresh(&mut measured, &workspace, &index, &GraphOptions::default());
    let replacement_peak = measured.metrics().peak_staging_bytes();

    let mut exact = StructureGraphProvider::new();
    refresh(&mut exact, &workspace, &index, &GraphOptions::default());
    exact
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: replacement_peak,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(exact.metrics().peak_staging_bytes(), replacement_peak);

    let mut below = StructureGraphProvider::new();
    refresh(&mut below, &workspace, &index, &GraphOptions::default());
    let published = below.graph().unwrap().clone();
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: replacement_peak - 1,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
    ));
    assert_eq!(below.graph(), Some(&published));
}

#[test]
fn long_retained_paths_and_counts_fit_the_native_replacement_envelope_exactly() {
    let fixture = Fixture::new();
    let directory = "long-segment-".repeat(12);
    let manifest = format!("{directory}/Cargo.toml");
    fixture.write(
        &manifest,
        "[package]\nname=\"long-path\"\nversion=\"0.1.0\"\n",
    );
    for index in 0..128 {
        fixture.write(
            &format!(
                "{directory}/src/item_{index:03}_{}.rs",
                "long-name-".repeat(12)
            ),
            format!("pub fn item_{index}() {{}}\n"),
        );
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut baseline = StructureGraphProvider::new();
    refresh(&mut baseline, &workspace, &index, &GraphOptions::default());
    refresh(&mut baseline, &workspace, &index, &GraphOptions::default());
    let peak = baseline.metrics().peak_staging_bytes();
    assert!(peak <= GraphOptions::default().max_staging_bytes);

    let mut exact = StructureGraphProvider::new();
    refresh(&mut exact, &workspace, &index, &GraphOptions::default());
    exact
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: peak,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    let mut below = StructureGraphProvider::new();
    refresh(&mut below, &workspace, &index, &GraphOptions::default());
    assert!(matches!(
        below.refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_staging_bytes: peak - 1,
                ..GraphOptions::default()
            },
            &[],
            &[],
        ),
        Err(GraphError::BoundExceeded(GraphBound::StagingBytes))
    ));
}

#[test]
fn cache_shrink_evicts_stale_fragments_with_one_lru_pass() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let initial_entries = provider.cache_usage().entries();
    assert!(initial_entries > 2);

    fs::remove_file(fixture.workspace_path.join("crates/helper/Cargo.toml")).unwrap();
    fs::remove_file(fixture.workspace_path.join("crates/helper/src/lib.rs")).unwrap();
    fixture.write(
        "Cargo.toml",
        "[package]\nname=\"root\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    );
    let index = build_index(&workspace);
    provider
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_cache_entries: 18,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(provider.cache_usage().entries(), 18);
    assert!(provider.metrics().evicted_fragments() >= initial_entries - 18);
}

#[test]
fn cold_manifest_cache_admission_does_not_rescan_fitting_entries() {
    let fixture = Fixture::new();
    for index in 0..100 {
        fixture.write(
            &format!("p{index}/Cargo.toml"),
            format!("[package]\nname=\"p{index}\"\nversion=\"0.1.0\"\n"),
        );
        fixture.write(&format!("p{index}/src/lib.rs"), "pub fn item() {}\n");
    }
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    provider
        .refresh(
            &workspace,
            &index,
            &GraphOptions {
                max_work: 100_000,
                ..GraphOptions::default()
            },
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(provider.cache_usage().entries(), 400);
}

#[test]
fn node_edge_cache_staging_work_and_deadline_bounds_are_atomic() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut baseline = StructureGraphProvider::new();
    let graph = refresh(&mut baseline, &workspace, &index, &GraphOptions::default()).clone();
    let usage = baseline.cache_usage();
    let cases = [
        (
            GraphOptions {
                max_nodes: graph.nodes().len() - 1,
                ..GraphOptions::default()
            },
            GraphBound::Nodes,
        ),
        (
            GraphOptions {
                max_edges: graph.edges().len() - 1,
                ..GraphOptions::default()
            },
            GraphBound::Edges,
        ),
        (
            GraphOptions {
                max_cache_entries: 1,
                ..GraphOptions::default()
            },
            GraphBound::CacheEntries,
        ),
        (
            GraphOptions {
                max_cache_bytes: usage.logical_bytes() - 1,
                ..GraphOptions::default()
            },
            GraphBound::CacheBytes,
        ),
        (
            GraphOptions {
                max_staging_bytes: 1,
                ..GraphOptions::default()
            },
            GraphBound::StagingBytes,
        ),
        (
            GraphOptions {
                max_work: 1,
                ..GraphOptions::default()
            },
            GraphBound::Work,
        ),
        (
            GraphOptions {
                max_time: Duration::from_nanos(1),
                ..GraphOptions::default()
            },
            GraphBound::Time,
        ),
    ];
    for (options, expected) in cases {
        let mut provider = StructureGraphProvider::new();
        assert!(matches!(
            provider.refresh(&workspace, &index, &options, &[], &[]),
            Err(GraphError::BoundExceeded(actual)) if actual == expected
        ));
        assert!(provider.graph().is_none());
    }
}

#[test]
fn multiple_manifest_sections_for_one_import_retain_independent_provenance() {
    let fixture = Fixture::cargo_workspace();
    let workspace = fixture.open();
    let index = build_index(&workspace);
    let mut provider = StructureGraphProvider::new();
    let graph = refresh(&mut provider, &workspace, &index, &GraphOptions::default());
    let root = graph_node(graph, NodeKind::Package, "root").id();
    let helper = graph_node(graph, NodeKind::Package, "helper").id();
    let imports = graph
        .edges()
        .iter()
        .filter(|edge| {
            edge.source() == root && edge.target() == helper && edge.kind() == EdgeKind::Imports
        })
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2);
    assert_eq!(
        imports
            .iter()
            .map(|edge| edge.provenance().evidence_digest())
            .collect::<BTreeSet<_>>()
            .len(),
        imports.len()
    );
}

#[test]
fn pins_declare_exact_toml_globset_and_lock_checksums() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    assert_eq!(
        manifest["dependencies"]["toml"]["version"].as_str(),
        Some("=1.1.4")
    );
    assert_eq!(
        manifest["dependencies"]["globset"].as_str(),
        Some("=0.4.19")
    );
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.lock")).unwrap()).unwrap();
    let packages = lock["package"].as_array().unwrap();
    let toml = packages
        .iter()
        .find(|package| package["name"].as_str() == Some("toml"))
        .unwrap();
    assert_eq!(toml["version"].as_str(), Some("1.1.4+spec-1.1.0"));
    assert_eq!(
        toml["checksum"].as_str(),
        Some("3aace63f4bbcdfc2c965b059de67119c89c4017a70d633be6c104910f67056f5")
    );
    let globset = packages
        .iter()
        .find(|package| package["name"].as_str() == Some("globset"))
        .unwrap();
    assert_eq!(globset["version"].as_str(), Some("0.4.19"));
    assert_eq!(
        globset["checksum"].as_str(),
        Some("e47d37d2ae4464254884b60ab7071be2b876a9c35b696bd018ddcc76847309cd")
    );
}
