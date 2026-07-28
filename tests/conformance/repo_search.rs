#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use kit::workspace::{
    index::meta::{ContentState, IndexError, IndexOptions, MetadataIndex},
    revision::{ManagedWorkspace, RevisionError, RevisionOptions},
    search::lexical::{SearchError, SearchMode, SearchOptions, SearchQuery, search},
};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-repo-search-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace = root.join("workspace");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&workspace).unwrap();
        Self { root, workspace }
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.workspace.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn open(&self) -> ManagedWorkspace {
        ManagedWorkspace::open_with_options(
            &self.workspace,
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

    fn index(&self) -> (ManagedWorkspace, MetadataIndex) {
        let workspace = self.open();
        let revision = workspace.current_revision().unwrap();
        let index =
            MetadataIndex::build(&workspace, revision.id(), &IndexOptions::default()).unwrap();
        (workspace, index)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn paths(index: &MetadataIndex) -> Vec<String> {
    index
        .entries()
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect()
}

fn query(text: &str, mode: SearchMode) -> SearchQuery {
    SearchQuery {
        text: text.to_owned(),
        mode,
    }
}

#[test]
fn gitignore_core_semantics_have_zero_ignored_results() {
    let fixture = Fixture::new();
    fixture.write(
        ".gitignore",
        b"# comment\n   \n*.log\n/rooted.txt\ncache/\ndocs/*.tmp\ndeep/**/generated?.rs\nnegated-*\n!negated-keep   \ntrimmed   \nescaped\\ \nspace-dir/   \n\\!literal\n\\#hash\n",
    );
    for path in [
        "app.log",
        "nested/app.log",
        "rooted.txt",
        "cache/value.txt",
        "nested/cache/value.txt",
        "docs/one.tmp",
        "deep/generated1.rs",
        "deep/a/b/generated2.rs",
        "negated-drop",
        "trimmed",
        "escaped ",
        "space-dir/value.txt",
        "!literal",
        "#hash",
    ] {
        fixture.write(path, b"forbidden needle");
    }
    for path in [
        "nested/rooted.txt",
        "docs/deep/one.tmp",
        "deep/generated12.rs",
        "negated-keep",
        "trimmed   ",
        "escaped",
        "src/main.rs",
    ] {
        fixture.write(path, b"allowed needle");
    }
    fixture.write("src/.gitignore", b"*.tmp\n!keep.tmp\n");
    fixture.write("src/drop.tmp", b"forbidden needle");
    fixture.write("src/keep.tmp", b"allowed needle");

    let (workspace, index) = fixture.index();
    let indexed = paths(&index);
    for ignored in [
        "app.log",
        "nested/app.log",
        "rooted.txt",
        "cache",
        "cache/value.txt",
        "nested/cache",
        "nested/cache/value.txt",
        "docs/one.tmp",
        "deep/generated1.rs",
        "deep/a/b/generated2.rs",
        "negated-drop",
        "trimmed",
        "escaped ",
        "space-dir",
        "space-dir/value.txt",
        "!literal",
        "#hash",
        "src/drop.tmp",
    ] {
        assert!(
            !indexed.iter().any(|path| path == ignored),
            "ignored path returned: {ignored}"
        );
    }
    for included in [
        "nested/rooted.txt",
        "docs/deep/one.tmp",
        "deep/generated12.rs",
        "negated-keep",
        "trimmed   ",
        "escaped",
        "src/keep.tmp",
    ] {
        assert!(
            indexed.iter().any(|path| path == included),
            "missing included path: {included}"
        );
    }

    let response = search(
        &workspace,
        &index,
        &query("forbidden", SearchMode::Content),
        &SearchOptions::default(),
        None,
    )
    .unwrap();
    assert!(
        response.matches.is_empty(),
        "ignored content leaked from index"
    );
}

#[test]
fn ignored_parent_cannot_be_reincluded_and_private_metadata_is_absent() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", b"blocked/\n!blocked/keep.rs\n");
    fixture.write("blocked/keep.rs", b"needle");
    fixture.write(".git/config", b"needle");
    fixture.write(".kit/index", b"needle");
    fixture.write(".kit-revision-private", b"needle");
    fixture.write("visible.rs", b"needle");

    let (_, index) = fixture.index();
    assert_eq!(
        paths(&index)
            .iter()
            .filter(|path| path.ends_with("visible.rs"))
            .count(),
        1
    );
    assert!(paths(&index).iter().all(|path| {
        !path.starts_with("blocked")
            && path != ".git"
            && !path.starts_with(".git/")
            && !path.starts_with(".kit")
    }));
}

#[test]
fn unsupported_or_invalid_ignore_syntax_fails_explicitly() {
    for bytes in [b"file[0].rs\n".as_slice(), b"bad\\q\n", b"\xff\n"] {
        let fixture = Fixture::new();
        fixture.write(".gitignore", bytes);
        fixture.write("visible", b"ok");
        let workspace = fixture.open();
        let revision = workspace.current_revision().unwrap();
        assert!(matches!(
            MetadataIndex::build(&workspace, revision.id(), &IndexOptions::default()),
            Err(IndexError::InvalidIgnore { .. })
        ));
    }

    for options in [
        IndexOptions {
            max_ignore_rules: 1,
            ..IndexOptions::default()
        },
        IndexOptions {
            max_compiled_ignore_bytes: 1,
            ..IndexOptions::default()
        },
        IndexOptions {
            max_pattern_components: 1,
            ..IndexOptions::default()
        },
    ] {
        let fixture = Fixture::new();
        fixture.write(".gitignore", b"one\na/b\n");
        fixture.write("visible", b"ok");
        let workspace = fixture.open();
        let revision = workspace.current_revision().unwrap();
        assert!(matches!(
            MetadataIndex::build(&workspace, revision.id(), &options),
            Err(IndexError::IgnoreLimit(_))
        ));
    }

    let fixture = Fixture::new();
    fixture.write("visible", b"ok");
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap();
    assert!(matches!(
        MetadataIndex::build(
            &workspace,
            revision.id(),
            &IndexOptions {
                max_matcher_work_bytes: 1,
                ..IndexOptions::default()
            },
        ),
        Err(IndexError::IgnoreLimit(_))
    ));
    assert!(matches!(
        MetadataIndex::build(
            &workspace,
            revision.id(),
            &IndexOptions {
                max_build_time: Duration::from_nanos(1),
                ..IndexOptions::default()
            },
        ),
        Err(IndexError::DeadlineExceeded)
    ));
}

#[test]
fn metadata_is_canonical_bounded_and_classifies_text_binary_and_utf8() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "pub fn café() {}\r\nstruct Thing;\r\n");
    fixture.write("script.sh", b"#!/bin/sh\necho yes\n");
    fixture.write("binary", b"abc\0needle");
    fixture.write("invalid", b"abc\xffneedle");
    fixture.write("large.txt", vec![b'x'; 64]);
    fs::set_permissions(
        fixture.workspace.join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let workspace = fixture.open();
    let revision = workspace.current_revision().unwrap();
    let index = MetadataIndex::build(
        &workspace,
        revision.id(),
        &IndexOptions {
            max_file_bytes: 32,
            ..IndexOptions::default()
        },
    )
    .unwrap();

    let rust = index
        .entries()
        .iter()
        .find(|entry| entry.path == Path::new("src/lib.rs"))
        .unwrap();
    assert_eq!(rust.language.as_deref(), Some("rust"));
    assert_eq!(rust.content_state, ContentState::TooLarge);
    let script = index
        .entries()
        .iter()
        .find(|entry| entry.path == Path::new("script.sh"))
        .unwrap();
    assert!(script.executable);
    assert_eq!(script.language.as_deref(), Some("shell"));
    assert_eq!(
        index
            .entries()
            .iter()
            .find(|entry| entry.path == Path::new("binary"))
            .unwrap()
            .content_state,
        ContentState::Binary
    );
    assert_eq!(
        index
            .entries()
            .iter()
            .find(|entry| entry.path == Path::new("invalid"))
            .unwrap()
            .content_state,
        ContentState::InvalidUtf8
    );
    assert_eq!(
        index
            .entries()
            .iter()
            .find(|entry| entry.path == Path::new("large.txt"))
            .unwrap()
            .content_state,
        ContentState::TooLarge
    );
    assert!(index.entries().iter().all(|entry| {
        !entry.path.is_absolute()
            && entry
                .path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    }));
}

#[test]
fn unicode_crlf_symbols_and_deep_paths_are_searchable_without_binary_leaks() {
    let fixture = Fixture::new();
    let mut deep = String::new();
    for level in 0..32 {
        deep.push_str(&format!("d{level}/"));
    }
    deep.push_str("unicode.rs");
    fixture.write(
        &deep,
        "pub fn café() {}\r\nlet value = \"東京 needle\";\r\n",
    );
    let mut late_binary = vec![b'x'; 1536 * 1024];
    late_binary.extend_from_slice(b"\0needle");
    fixture.write("binary", late_binary);
    fixture.write("invalid", b"needle\xff");
    let (workspace, index) = fixture.index();
    let entry = index
        .entries()
        .iter()
        .find(|entry| entry.path == Path::new(&deep))
        .unwrap();
    assert_eq!(entry.symbols[0].name, "café");

    let response = search(
        &workspace,
        &index,
        &query("東京 needle", SearchMode::Content),
        &SearchOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(response.matches.len(), 1);
    assert_eq!(response.matches[0].line, Some(2));
    assert!(!response.matches[0].excerpt.ends_with('\r'));

    let skipped = search(
        &workspace,
        &index,
        &query("needle", SearchMode::Content),
        &SearchOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(skipped.skipped.binary, 1);
    assert_eq!(skipped.skipped.invalid_utf8, 1);
    assert!(
        skipped
            .matches
            .iter()
            .all(|found| found.path == Path::new(&deep))
    );
}

#[test]
fn ranking_and_ties_are_deterministic_and_filters_apply() {
    let fixture = Fixture::new();
    fixture.write("needle", b"none");
    fixture.write("a/needle.rs", b"needle");
    fixture.write("b/needle.rs", b"needle");
    fixture.write("boundaries.rs", "αtoken token tokenβ");
    fixture.write("z.txt", b"needle");
    let (workspace, first) = fixture.index();
    let revision = workspace.current_revision().unwrap();
    let second = MetadataIndex::build(&workspace, revision.id(), &IndexOptions::default()).unwrap();
    let options = SearchOptions {
        languages: vec!["rust".to_owned()],
        ..SearchOptions::default()
    };
    let run = |index: &MetadataIndex| {
        search(
            &workspace,
            index,
            &query("needle", SearchMode::PathAndContent),
            &options,
            None,
        )
        .unwrap()
        .matches
    };
    let left = run(&first);
    let right = run(&second);
    assert_eq!(left, right);
    assert_eq!(left[0].path, Path::new("a/needle.rs"));
    assert_eq!(left[1].path, Path::new("b/needle.rs"));
    assert!(
        left.iter()
            .all(|found| found.path.extension().is_some_and(|value| value == "rs"))
    );

    let boundaries = search(
        &workspace,
        &first,
        &query("token", SearchMode::Content),
        &options,
        None,
    )
    .unwrap()
    .matches;
    assert_eq!(
        boundaries
            .iter()
            .map(|found| found.score)
            .collect::<Vec<_>>(),
        vec![800, 600, 600]
    );
    assert_eq!(boundaries[0].byte_start, "αtoken ".len());
}

#[test]
fn every_search_bound_marks_truncation_and_output_never_exceeds_limits() {
    let fixture = Fixture::new();
    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        fixture.write(name, b"needle needle needle\n");
    }
    let (workspace, index) = fixture.index();
    let request = query("needle", SearchMode::Content);

    let count_options = SearchOptions {
        max_results: 2,
        ..SearchOptions::default()
    };
    let mut cursor = None;
    let mut paged = Vec::new();
    for _ in 0..7 {
        let page = search(
            &workspace,
            &index,
            &request,
            &count_options,
            cursor.as_ref(),
        )
        .unwrap();
        assert!(!page.matches.is_empty(), "a cursor must always advance");
        paged.extend(
            page.matches
                .iter()
                .map(|found| (found.path.clone(), found.byte_start)),
        );
        assert_eq!(page.omitted, 12 - paged.len());
        assert!(page.omitted_complete);
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none(), "paging must terminate");
    let mut unique = paged.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(paged.len(), 12);
    assert_eq!(unique.len(), paged.len(), "paging returned duplicates");

    let exact_options = SearchOptions {
        max_results: 1,
        ..SearchOptions::default()
    };
    let baseline = search(&workspace, &index, &request, &exact_options, None).unwrap();
    assert_eq!(
        baseline.result_bytes,
        baseline.to_canonical_json().unwrap().len()
    );
    let exact_budget = baseline.result_bytes;
    let bytes_options = SearchOptions {
        max_result_bytes: exact_budget,
        ..exact_options
    };
    let bytes = search(&workspace, &index, &request, &bytes_options, None).unwrap();
    assert_eq!(bytes.matches.len(), 1);
    assert!(bytes.truncated && bytes.cursor.is_some());
    assert_eq!(bytes.omitted, 11);
    assert!(bytes.omitted_complete);
    assert_eq!(bytes.result_bytes, bytes_options.max_result_bytes);
    assert_eq!(
        bytes.result_bytes,
        serde_json::to_vec(&bytes).unwrap().len()
    );
    assert_eq!(
        bytes.to_canonical_json().unwrap(),
        serde_json::to_vec(&bytes).unwrap()
    );
    let one_byte_short = search(
        &workspace,
        &index,
        &request,
        &SearchOptions {
            max_result_bytes: bytes_options.max_result_bytes - 1,
            ..bytes_options.clone()
        },
        None,
    )
    .unwrap();
    assert!(one_byte_short.matches.is_empty());
    assert!(one_byte_short.truncated && one_byte_short.cursor.is_none());
    assert_eq!(one_byte_short.omitted, 12);
    assert!(one_byte_short.omitted_complete);
    assert_eq!(
        one_byte_short.result_bytes,
        serde_json::to_vec(&one_byte_short).unwrap().len()
    );

    let files = search(
        &workspace,
        &index,
        &request,
        &SearchOptions {
            max_scanned_files: 1,
            ..SearchOptions::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(files.scanned_files, 1);
    assert!(files.truncated && !files.omitted_complete && files.cursor.is_none());

    let scanned_bytes = search(
        &workspace,
        &index,
        &request,
        &SearchOptions {
            max_scanned_bytes: 20,
            ..SearchOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(scanned_bytes.scanned_bytes <= 20);
    assert!(scanned_bytes.truncated && scanned_bytes.cursor.is_none());

    let snippets = search(
        &workspace,
        &index,
        &request,
        &SearchOptions {
            max_snippet_bytes: 6,
            ..SearchOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(snippets.truncated && snippets.cursor.is_none());
    assert!(snippets.omitted_complete);
    assert!(snippets.matches.iter().all(|found| found.excerpt_truncated));

    assert!(matches!(
        search(
            &workspace,
            &index,
            &request,
            &SearchOptions {
                max_time: Duration::from_nanos(1),
                ..SearchOptions::default()
            },
            None,
        ),
        Err(SearchError::TimeLimit)
    ));

    let revision = workspace.current_revision().unwrap();
    let limited_index = MetadataIndex::build(
        &workspace,
        revision.id(),
        &IndexOptions {
            max_entries: 1,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    let limited = search(
        &workspace,
        &limited_index,
        &request,
        &SearchOptions::default(),
        None,
    )
    .unwrap();
    assert!(limited.truncated && limited.cursor.is_none());

    let escaped_fixture = Fixture::new();
    escaped_fixture.write("quote\"\\snow-雪.rs", "needle \"\\\t\u{0001}雪");
    let (escaped_workspace, escaped_index) = escaped_fixture.index();
    let escaped = search(
        &escaped_workspace,
        &escaped_index,
        &request,
        &SearchOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(escaped.matches.len(), 1);
    let escaped_json = escaped.to_canonical_json().unwrap();
    assert_eq!(escaped.result_bytes, escaped_json.len());
    let escaped_text = std::str::from_utf8(&escaped_json).unwrap();
    assert!(escaped_text.contains("quote\\\"\\\\snow-雪.rs"));
    assert!(escaped_text.contains("needle \\\"\\\\\\t\\u0001雪"));
    let escaped_budget = escaped.result_bytes;
    let exact_escape = search(
        &escaped_workspace,
        &escaped_index,
        &request,
        &SearchOptions {
            max_result_bytes: escaped_budget,
            ..SearchOptions::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(exact_escape.matches.len(), 1);
    assert_eq!(exact_escape.result_bytes, escaped_budget);
    assert_eq!(
        exact_escape.result_bytes,
        serde_json::to_vec(&exact_escape).unwrap().len()
    );
    let short_escape = search(
        &escaped_workspace,
        &escaped_index,
        &request,
        &SearchOptions {
            max_result_bytes: escaped_budget - 1,
            ..SearchOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(short_escape.matches.is_empty() && short_escape.cursor.is_none());
    assert_eq!(short_escape.omitted, 1);
    assert!(short_escape.omitted_complete);

    let mut widest = bytes;
    widest.scanned_files = usize::MAX;
    widest.scanned_bytes = u64::MAX;
    widest.skipped.binary = usize::MAX;
    widest.skipped.invalid_utf8 = usize::MAX;
    widest.skipped.too_large = usize::MAX;
    widest.skipped.index_limited = usize::MAX;
    widest.omitted = usize::MAX;
    widest.matches[0].score = u16::MAX;
    widest.matches[0].line = Some(usize::MAX);
    widest.matches[0].column = Some(usize::MAX);
    widest.matches[0].byte_start = usize::MAX;
    widest.matches[0].byte_end = usize::MAX;
    widest.result_bytes = 0;
    loop {
        let actual = serde_json::to_vec(&widest).unwrap().len();
        if widest.result_bytes == actual {
            break;
        }
        widest.result_bytes = actual;
    }
    let widest_json = widest.to_canonical_json().unwrap();
    assert_eq!(widest.result_bytes, widest_json.len());
    let widest_value: serde_json::Value = serde_json::from_slice(&widest_json).unwrap();
    let cursor = widest_value["cursor"].as_object().unwrap();
    assert_eq!(cursor["index_digest"].as_str().unwrap().len(), 64);
    assert_eq!(cursor["query_digest"].as_str().unwrap().len(), 64);
    assert_eq!(cursor["options_digest"].as_str().unwrap().len(), 64);
}

#[test]
fn invalid_queries_and_options_are_rejected() {
    let fixture = Fixture::new();
    fixture.write("file", b"needle");
    let (workspace, index) = fixture.index();
    assert!(matches!(
        search(
            &workspace,
            &index,
            &query("", SearchMode::Content),
            &SearchOptions::default(),
            None
        ),
        Err(SearchError::InvalidQuery(_))
    ));
    assert!(matches!(
        search(
            &workspace,
            &index,
            &query("needle", SearchMode::Content),
            &SearchOptions {
                path_prefixes: vec![PathBuf::from("../escape")],
                ..SearchOptions::default()
            },
            None,
        ),
        Err(SearchError::InvalidOptions(_))
    ));
}

#[test]
fn index_and_search_cursors_are_stale_after_revision_change() {
    let fixture = Fixture::new();
    fixture.write("a", b"needle");
    fixture.write("b", b"needle");
    let (workspace, index) = fixture.index();
    let index_cursor = index.cursor();
    let options = SearchOptions {
        max_results: 1,
        ..SearchOptions::default()
    };
    let response = search(
        &workspace,
        &index,
        &query("needle", SearchMode::Content),
        &options,
        None,
    )
    .unwrap();
    let other_index = MetadataIndex::build(
        &workspace,
        index.revision(),
        &IndexOptions {
            max_symbols_per_file: 1,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    assert!(matches!(
        other_index.validate_cursor(&workspace, &index_cursor),
        Err(IndexError::CursorMismatch)
    ));
    assert!(matches!(
        search(
            &workspace,
            &index,
            &query("different", SearchMode::Content),
            &options,
            response.cursor.as_ref(),
        ),
        Err(SearchError::CursorMismatch)
    ));
    assert!(matches!(
        search(
            &workspace,
            &index,
            &query("needle", SearchMode::Content),
            &SearchOptions {
                max_results: 2,
                ..SearchOptions::default()
            },
            response.cursor.as_ref(),
        ),
        Err(SearchError::CursorMismatch)
    ));
    fixture.write("a", b"changed");

    assert!(matches!(
        index.validate_cursor(&workspace, &index_cursor),
        Err(IndexError::Revision(RevisionError::StaleRevision { .. }))
    ));
    assert!(matches!(
        search(
            &workspace,
            &index,
            &query("needle", SearchMode::Content),
            &options,
            response.cursor.as_ref(),
        ),
        Err(SearchError::Revision(RevisionError::StaleRevision { .. }))
    ));
}

#[test]
fn active_writer_never_allows_results_from_an_old_revision() {
    let fixture = Fixture::new();
    const FILE_BYTES: usize = 256 * 1024;
    for index in 0..8 {
        fixture.write(&format!("file-{index:02}"), vec![b'a'; FILE_BYTES]);
    }
    let (workspace, index) = fixture.index();
    let workspace = Arc::new(workspace);
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let path = fixture.workspace.join("file-00");
    let writer = thread::spawn(move || {
        let mut value = b'b';
        while !writer_stop.load(Ordering::Acquire) {
            fs::write(&path, vec![value; FILE_BYTES]).unwrap();
            value = if value == b'b' { b'c' } else { b'b' };
        }
    });
    thread::sleep(Duration::from_millis(5));
    let result = search(
        &workspace,
        &index,
        &query("not-present", SearchMode::Content),
        &SearchOptions {
            max_time: Duration::from_secs(5),
            ..SearchOptions::default()
        },
        None,
    );
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(matches!(
        result,
        Err(SearchError::Revision(
            RevisionError::StaleRevision { .. } | RevisionError::ScanRace { .. }
        ))
    ));
}
