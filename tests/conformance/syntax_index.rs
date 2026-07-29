#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use kit::workspace::{
    index::meta::{IndexError, IndexOptions, MetadataIndex},
    revision::{ManagedWorkspace, RevisionOptions},
    search::discover::{DiscoverOptions, DiscoverQuery, discover},
    syntax::{
        FactSource, LanguageDescriptor, RUST_GRAMMAR_ABI, RUST_GRAMMAR_ARTIFACT_DIGEST,
        RUST_GRAMMAR_VERSION, RUST_QUERY, RUST_QUERY_SET_DIGEST, SyntacticFacts,
        SyntacticProvenance, SyntaxIndex, TREE_SITTER_RUNTIME_VERSION, UnavailableReason,
    },
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-syntax-index-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&workspace_path).unwrap();
        Self {
            root,
            workspace_path,
        }
    }

    fn write(&self, path: &str, source: impl AsRef<[u8]>) {
        let path = self.workspace_path.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, source).unwrap();
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn snapshot_bound_rust_records_have_canonical_workspace_provenance() {
    let fixture = Fixture::new();
    fixture.write(
        "src/lib.rs",
        "mod outer { struct Café; impl Café { fn brew(&self) {} } }\n",
    );
    fixture.write("tool.py", "def fallback():\n    pass\n");
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap();
    let mut syntax = SyntaxIndex::new();
    let index = MetadataIndex::build_with_syntax(
        &workspace,
        revision.id(),
        &IndexOptions::default(),
        &mut syntax,
    )
    .unwrap();
    let rust = index
        .entries()
        .iter()
        .find(|entry| entry.path == Path::new("src/lib.rs"))
        .unwrap();
    let names = rust
        .syntax_records
        .iter()
        .map(|record| record.qualified_name().value().as_ref())
        .collect::<Vec<_>>();
    assert_eq!(names, ["outer", "outer::Café", "outer::Café::brew"]);
    assert!(rust.symbols.is_empty());
    let mut grammar_hash = blake3::Hasher::new();
    grammar_hash.update(b"kit-syntax-grammar-v2\0");
    grammar_hash.update(&(RUST_GRAMMAR_ABI as u128).to_le_bytes());
    for value in [RUST_GRAMMAR_VERSION, RUST_GRAMMAR_ARTIFACT_DIGEST] {
        grammar_hash.update(&(value.len() as u64).to_le_bytes());
        grammar_hash.update(value.as_bytes());
    }
    let expected_grammar = *grammar_hash.finalize().as_bytes();
    let expected_query = LanguageDescriptor::rust().query_set_digest();
    for record in rust.syntax_records.iter() {
        assert_eq!(record.workspace_revision(), revision.id());
        assert_eq!(record.canonical_path(), Path::new("src/lib.rs"));
        let assert_provenance = |provenance: &SyntacticProvenance| {
            assert_eq!(provenance.source(), FactSource::Syntactic);
            assert_eq!(provenance.confidence_millis(), 1_000);
            assert_eq!(provenance.revision(), revision.id());
            assert_eq!(provenance.range(), record.range());
            assert_eq!(provenance.grammar_identity(), expected_grammar);
            assert_eq!(provenance.query_set_digest(), expected_query);
        };
        assert_provenance(record.qualified_name().provenance());
        assert_provenance(record.display_name().provenance());
        assert_provenance(record.kind().provenance());
        assert_provenance(record.signature().provenance());
        assert_provenance(record.declaration().provenance());
        if let Some(enclosing) = record.enclosing_symbol() {
            assert_provenance(enclosing.provenance());
        }
        let SyntacticFacts::Available(definitions) = record.definitions() else {
            panic!("canonical definition fact must be available");
        };
        for definition in definitions.iter() {
            assert_provenance(definition.provenance());
        }
        assert!(matches!(
            record.imports(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
        assert!(matches!(
            record.exports(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
        assert!(matches!(
            record.references(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
        assert!(matches!(
            record.callers(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
        assert!(matches!(
            record.callees(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
        assert!(matches!(
            record.tests(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
        assert!(matches!(
            record.documentation(),
            SyntacticFacts::Unavailable(UnavailableReason::NotExtracted)
        ));
    }
    assert!(
        rust.syntax_records
            .iter()
            .any(|record| record.enclosing_symbol().is_some())
    );
    let python = index
        .entries()
        .iter()
        .find(|entry| entry.path == Path::new("tool.py"))
        .unwrap();
    assert!(python.syntax_records.is_empty());
    assert_eq!(python.symbols[0].name, "fallback");
}

#[test]
fn malformed_snapshot_owners_do_not_publish_exact_children() {
    let fixture = Fixture::new();
    fixture.write(
        "src/lib.rs",
        "fn broken() { fn child() {} let = ; }\nfn clean() {}\n",
    );
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let index = MetadataIndex::build(&workspace, revision, &IndexOptions::default()).unwrap();
    let entry = index
        .entries()
        .iter()
        .find(|entry| entry.path == Path::new("src/lib.rs"))
        .unwrap();
    assert!(entry.syntax_has_parse_errors);
    assert!(entry.syntax_rejected_malformed > 0);
    assert!(
        entry
            .syntax_records
            .iter()
            .all(|record| record.display_name().value().as_ref() != "child")
    );
    assert!(
        entry
            .syntax_records
            .iter()
            .any(|record| record.display_name().value().as_ref() == "clean")
    );
}

#[test]
fn complete_scan_prunes_deleted_ignored_binary_oversized_and_non_rust_paths() {
    let fixture = Fixture::new();
    for path in [
        "keep.rs",
        "deleted.rs",
        "ignored.rs",
        "binary.rs",
        "large.rs",
    ] {
        fixture.write(path, format!("fn {}() {{}}\n", path.replace('.', "_")));
    }
    let workspace = fixture.open();
    let first = workspace.current_revision().unwrap().id();
    let mut syntax = SyntaxIndex::new();
    MetadataIndex::build_with_syntax(&workspace, first, &IndexOptions::default(), &mut syntax)
        .unwrap();
    assert_eq!(syntax.cache_usage().resident_files, 5);

    fs::remove_file(fixture.workspace_path.join("deleted.rs")).unwrap();
    fixture.write(".gitignore", "ignored.rs\n");
    fixture.write("binary.rs", b"fn binary() {}\0");
    fixture.write("large.rs", "x".repeat(256));
    fixture.write("keep.rs", "fn keep() {}\nfn extra() {}\n");
    let second = workspace.current_revision().unwrap().id();
    let index = MetadataIndex::build_with_syntax(
        &workspace,
        second,
        &IndexOptions {
            max_file_bytes: 128,
            max_symbols_per_file: 1,
            ..IndexOptions::default()
        },
        &mut syntax,
    )
    .unwrap();
    assert!(index.truncated(), "per-file syntax output should be marked");
    assert_eq!(syntax.cache_usage().resident_files, 1);
    assert!(syntax.metrics().pruned_files >= 4);
}

#[test]
fn globally_incomplete_scan_does_not_prune_unvisited_rust_residents() {
    let fixture = Fixture::new();
    fixture.write("a.rs", "fn a() {}\n");
    fixture.write("b.rs", "fn b() {}\n");
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let mut syntax = SyntaxIndex::new();
    MetadataIndex::build_with_syntax(&workspace, revision, &IndexOptions::default(), &mut syntax)
        .unwrap();
    let before = syntax.metrics().pruned_files;
    let limited = MetadataIndex::build_with_syntax(
        &workspace,
        revision,
        &IndexOptions {
            max_entries: 1,
            ..IndexOptions::default()
        },
        &mut syntax,
    )
    .unwrap();
    assert!(limited.truncated());
    assert_eq!(syntax.cache_usage().resident_files, 2);
    assert_eq!(syntax.metrics().pruned_files, before);
}

#[test]
fn snapshot_misses_cannot_evict_protected_residents() {
    let fixture = Fixture::new();
    fixture.write("a.rs", "fn a() {}\n");
    fixture.write("b.rs", "fn b() {}\n");
    let workspace = fixture.open();
    let first = workspace.current_revision().unwrap().id();
    let mut syntax = SyntaxIndex::with_cache_limits(kit::workspace::syntax::SyntaxCacheLimits {
        max_resident_files: 2,
        max_resident_logical_weight: 1024 * 1024,
        max_staging_files: 2,
        max_staging_logical_weight: 1024 * 1024,
        max_queries: 1,
        max_query_bytes: 64 * 1024,
    })
    .unwrap();
    MetadataIndex::build_with_syntax(&workspace, first, &IndexOptions::default(), &mut syntax)
        .unwrap();
    fixture.write("0.rs", "fn zero() {}\n");
    fixture.write("z.rs", "fn zed() {}\n");
    let second = workspace.current_revision().unwrap().id();
    MetadataIndex::build_with_syntax(&workspace, second, &IndexOptions::default(), &mut syntax)
        .unwrap();
    assert_eq!(syntax.cache_usage().resident_files, 2);
    assert_eq!(syntax.metrics().revision_refreshes, 2);
}

#[test]
fn metadata_deadline_is_typed_and_does_not_publish_cache_changes() {
    let fixture = Fixture::new();
    fixture.write("lib.rs", "fn ready() {}\n");
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let mut syntax = SyntaxIndex::new();
    MetadataIndex::build_with_syntax(&workspace, revision, &IndexOptions::default(), &mut syntax)
        .unwrap();
    let snapshot = workspace.snapshot(revision).unwrap();
    let usage = syntax.cache_usage();
    let metrics = syntax.metrics();
    assert!(matches!(
        MetadataIndex::from_snapshot_with_syntax(
            &snapshot,
            &IndexOptions {
                max_build_time: Duration::from_nanos(1),
                ..IndexOptions::default()
            },
            &mut syntax,
        ),
        Err(IndexError::DeadlineExceeded)
    ));
    assert_eq!(syntax.cache_usage(), usage);
    assert_eq!(syntax.metrics(), metrics);
}

#[test]
fn aggregate_metadata_syntax_limits_keep_a_useful_bounded_prefix() {
    let fixture = Fixture::new();
    for index in 0..40 {
        fixture.write(
            &format!("src/file_{index:03}.rs"),
            format!("fn useful_{index:03}() {{}}\n"),
        );
    }
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let options = IndexOptions {
        max_syntax_records: 5,
        max_syntax_logical_weight: 32 * 1024,
        ..IndexOptions::default()
    };
    let mut syntax = SyntaxIndex::new();
    let index =
        MetadataIndex::build_with_syntax(&workspace, revision, &options, &mut syntax).unwrap();
    assert_eq!(syntax.metrics().full_parses, 5);
    assert_eq!(syntax.metrics().reused, 0);
    assert_eq!(syntax.metrics().incremental_parses, 0);
    assert_eq!(syntax.metrics().revision_refreshes, 0);
    assert!(index.truncated());
    assert!(index.syntax_record_count() > 0);
    assert!(index.syntax_record_count() <= options.max_syntax_records);
    assert!(index.syntax_logical_weight() <= options.max_syntax_logical_weight);
    assert_eq!(
        index
            .entries()
            .iter()
            .map(|entry| entry.syntax_records.len())
            .sum::<usize>(),
        index.syntax_record_count()
    );
    assert!(
        index
            .entries()
            .iter()
            .any(|entry| entry.syntax_truncated && entry.syntax_records.is_empty())
    );
    let response = discover(
        &workspace,
        &index,
        &DiscoverQuery {
            terms: vec!["useful_000".to_owned()],
            roots: Vec::new(),
            languages: Vec::new(),
        },
        &DiscoverOptions::default(),
        None,
    )
    .unwrap();
    assert!(
        response
            .results
            .iter()
            .any(|result| result.symbol.as_deref() == Some("useful_000"))
    );
}

#[test]
fn aggregate_exhaustion_skips_parsing_and_preserves_eligible_residents() {
    let fixture = Fixture::new();
    for index in 0..10 {
        fixture.write(
            &format!("src/resident_{index:03}.rs"),
            format!("fn resident_{index:03}() {{}}\n"),
        );
    }
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap().id();
    let mut syntax = SyntaxIndex::new();
    MetadataIndex::build_with_syntax(&workspace, revision, &IndexOptions::default(), &mut syntax)
        .unwrap();
    assert_eq!(syntax.cache_usage().resident_files, 10);
    let before = syntax.metrics();
    MetadataIndex::build_with_syntax(
        &workspace,
        revision,
        &IndexOptions {
            max_syntax_records: 1,
            ..IndexOptions::default()
        },
        &mut syntax,
    )
    .unwrap();
    assert_eq!(syntax.cache_usage().resident_files, 10);
    assert_eq!(syntax.metrics().reused - before.reused, 1);
    assert_eq!(syntax.metrics().full_parses, before.full_parses);
    assert_eq!(
        syntax.metrics().revision_refreshes,
        before.revision_refreshes
    );
}

#[test]
fn exact_runtime_grammar_and_query_pins_are_bound() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let runtime = lock_package(&lock, "tree-sitter");
    let grammar = lock_package(&lock, "tree-sitter-rust");
    assert_eq!(runtime["version"], "0.25.10");
    assert_eq!(
        runtime["checksum"],
        "78f873475d258561b06f1c595d93308a7ed124d9977cb26b148c2084a4a3cc87"
    );
    assert_eq!(grammar["version"], "0.24.0");
    assert_eq!(grammar["checksum"], &RUST_GRAMMAR_ARTIFACT_DIGEST[7..]);
    let manifest: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(root.join("docs/compatibility/build-manifest.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest_pin(&manifest, "grammar.runtime"),
        "tree-sitter=0.25.10;crate_sha256=78f873475d258561b06f1c595d93308a7ed124d9977cb26b148c2084a4a3cc87"
    );
    assert_eq!(
        manifest_pin(&manifest, "grammar.languages"),
        "rust=tree-sitter-rust@0.24.0;abi=15;crate_sha256=4b9b18034c684a2420722be8b2a91c9c44f2546b631c039edf575ccba8c61be1"
    );
    assert_eq!(
        manifest_pin(&manifest, "grammar.queries"),
        format!("rust.query_set={RUST_QUERY_SET_DIGEST}")
    );
    assert_eq!(TREE_SITTER_RUNTIME_VERSION, runtime["version"]);
    assert_eq!(RUST_GRAMMAR_VERSION, "tree-sitter-rust@0.24.0");
    assert_eq!(
        LanguageDescriptor::rust().grammar_version(),
        RUST_GRAMMAR_VERSION
    );
    assert_eq!(
        LanguageDescriptor::rust().grammar_artifact_digest(),
        RUST_GRAMMAR_ARTIFACT_DIGEST
    );
    assert_eq!(LanguageDescriptor::rust().grammar_abi(), RUST_GRAMMAR_ABI);
    assert_eq!(RUST_GRAMMAR_ABI, 15);
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    assert_eq!(language.abi_version(), RUST_GRAMMAR_ABI);
    assert!(!RUST_QUERY.is_empty());
    assert_eq!(
        format!(
            "blake3:{}",
            blake3::Hash::from_bytes(LanguageDescriptor::rust().query_set_digest()).to_hex()
        ),
        RUST_QUERY_SET_DIGEST
    );
}

fn lock_package(lock: &str, wanted: &str) -> BTreeMap<String, String> {
    lock.split("[[package]]")
        .skip(1)
        .find_map(|section| {
            let values = section
                .lines()
                .filter_map(|line| {
                    let (key, value) = line.split_once(" = ")?;
                    Some((key.to_owned(), value.trim_matches('"').to_owned()))
                })
                .collect::<BTreeMap<_, _>>();
            (values.get("name").is_some_and(|name| name == wanted)).then_some(values)
        })
        .unwrap()
}

fn manifest_pin<'a>(manifest: &'a serde_yaml::Value, wanted: &str) -> &'a str {
    manifest["pins"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|pin| pin["id"].as_str() == Some(wanted))
        .unwrap()["value"]
        .as_str()
        .unwrap()
}
