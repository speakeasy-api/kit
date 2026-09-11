use super::{get, set, set_strings, unset};
use std::{fs, io::ErrorKind, path::Path};

fn document(path: &Path) -> toml::Value {
    toml::from_str(
        fs::read_to_string(path)
            .unwrap()
            .trim_start_matches('\u{feff}'),
    )
    .unwrap()
}

fn assert_unchanged(path: &Path, contents: &str, modified: std::time::SystemTime) {
    assert_eq!(fs::read_to_string(path).unwrap(), contents);
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
}

#[test]
fn all_toml_value_types_round_trip_through_api() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for (key, input) in [
        ("string", "\"hello\\nworld\""),
        ("integer", "0x2a"),
        ("float", "1.25e2"),
        ("boolean", "true"),
        ("offset_datetime", "1979-05-27T07:32:00Z"),
        ("local_datetime", "1979-05-27T07:32:00"),
        ("local_date", "1979-05-27"),
        ("local_time", "07:32:00"),
        ("array", "[1, \"two\", false, [3]]"),
        ("inline", "{ enabled = true, nested = { count = 2 } }"),
    ] {
        set(&path, key, input).unwrap();
        let expected: toml::Value = toml::from_str(&format!("value = {input}")).unwrap();
        assert_eq!(document(&path)[key], expected["value"], "{key}");
        let returned: toml::Value =
            toml::from_str(&format!("value = {}", get(&path, Some(key)).unwrap())).unwrap();
        assert_eq!(returned["value"], expected["value"], "{key}");
    }
}

#[test]
fn plain_models_and_explicit_strings_do_not_coerce() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for (input, expected) in [
        ("openai/gpt-5.4", "openai/gpt-5.4"),
        ("  ordinary text  ", "  ordinary text  "),
        ("\"true\"", "true"),
        ("'42'", "42"),
        ("\"[1, 2]\"", "[1, 2]"),
        ("\"1979-05-27\"", "1979-05-27"),
        ("", ""),
    ] {
        set(&path, "model", input).unwrap();
        assert_eq!(document(&path)["model"].as_str(), Some(expected));
    }
}

#[test]
fn nested_and_quoted_keys_are_paths_not_literal_dotted_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    set(&path, "providers.\"acme.ai\".'api key'", "secret").unwrap();
    set(&path, "\"literal.dot\"", "true").unwrap();
    assert_eq!(
        document(&path)["providers"]["acme.ai"]["api key"],
        toml::Value::from("secret")
    );
    assert_eq!(document(&path)["literal.dot"], toml::Value::from(true));
    assert_eq!(
        get(&path, Some("providers.'acme.ai'.\"api key\"")).unwrap(),
        "\"secret\""
    );
    unset(&path, "providers.'acme.ai'.'api key'").unwrap();
    assert_eq!(
        get(&path, Some("providers.'acme.ai'.'api key'"))
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn edits_arrays_inline_tables_tables_and_dotted_assignments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "array = [1, 2]\ninline = { keep = 7, old = true }\ndotted.old = 1\n[section]\nold = 2\nkeep = 'yes'\n").unwrap();
    set(&path, "array", "[false, { name = 'new' }]").unwrap();
    set(&path, "inline.old", "false").unwrap();
    set(&path, "inline.nested.deeper", "42").unwrap();
    set(&path, "dotted.old", "3").unwrap();
    set(&path, "dotted.new", "4").unwrap();
    set(&path, "section.old", "5").unwrap();
    let doc = document(&path);
    assert_eq!(doc["array"][0], toml::Value::from(false));
    assert_eq!(doc["array"][1]["name"], toml::Value::from("new"));
    assert_eq!(doc["inline"]["keep"], toml::Value::from(7));
    assert_eq!(doc["inline"]["old"], toml::Value::from(false));
    assert_eq!(doc["inline"]["nested"]["deeper"], toml::Value::from(42));
    assert_eq!(get(&path, Some("inline.nested.deeper")).unwrap(), "42");
    assert_eq!(doc["dotted"]["old"], toml::Value::from(3));
    assert_eq!(doc["dotted"]["new"], toml::Value::from(4));
    assert_eq!(doc["section"]["old"], toml::Value::from(5));
    assert_eq!(doc["section"]["keep"], toml::Value::from("yes"));
    unset(&path, "inline.nested.deeper").unwrap();
    unset(&path, "section").unwrap();
    assert!(document(&path).get("section").is_none());
    assert!(document(&path)["inline"]["nested"].get("deeper").is_none());
}

#[test]
fn replacing_existing_tables_with_values_produces_valid_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for source in [
        "[replace]\nold = 1\n[replace.child]\nold = 2\n[keep]\nvalue = 3\n",
        "replace.old = 1\nreplace.child.old = 2\nkeep.value = 3\n",
    ] {
        for replacement in ["false", "[1, 2]", "{ nested = { value = 4 } }"] {
            fs::write(&path, source).unwrap();
            set(&path, "replace", replacement).unwrap();
            let expected: toml::Value =
                toml::from_str(&format!("replace = {replacement}")).unwrap();
            assert_eq!(document(&path)["replace"], expected["replace"]);
            assert_eq!(document(&path)["keep"]["value"], toml::Value::from(3));
        }
    }
}

#[test]
fn bom_and_missing_model_are_preserved_without_initializing_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "\u{feff}# user config\nunknown = 'keep'\n";
    fs::write(&path, original).unwrap();
    assert_eq!(get(&path, None).unwrap(), original);
    assert_eq!(
        get(&path, Some("model")).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    set(&path, "model", "anthropic/claude-sonnet-4").unwrap();
    assert!(fs::read_to_string(&path).unwrap().starts_with(original));
    assert_eq!(document(&path).as_table().unwrap().len(), 2);
    unset(&path, "model").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn comments_spacing_and_unknown_sections_survive_targeted_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# heading\nmodel   = 'old' # selected\n\n[unknown] # extension\nodd = [ 1,  2, ] # keep\nopaque = { x = 'y' }\n";
    fs::write(&path, original).unwrap();
    set(&path, "model", "new/model").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        original.replace("'old'", "\"new/model\"")
    );
}

#[test]
fn missing_files_and_keys_obey_read_and_unset_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing/parents/config.toml");
    for key in [None, Some("model")] {
        assert_eq!(get(&path, key).unwrap_err().kind(), ErrorKind::NotFound);
    }
    unset(&path, "deep.absent").unwrap();
    assert!(!dir.path().join("missing").exists());
    set(&path, "model", "plain/model").unwrap();
    assert_eq!(document(&path).as_table().unwrap().len(), 1);
    let original = fs::read_to_string(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(get(&path, None).unwrap(), original);
    assert_eq!(get(&path, Some("model")).unwrap(), "\"plain/model\"");
    assert_eq!(
        get(&path, Some("absent.child")).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    unset(&path, "absent.child").unwrap();
    assert_unchanged(&path, &original, modified);
}

#[test]
fn malformed_documents_values_and_paths_never_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let malformed = "model = [\n";
    fs::write(&path, malformed).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(get(&path, None).unwrap_err().kind(), ErrorKind::InvalidData);
    assert_eq!(
        set(&path, "model", "new").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        unset(&path, "model").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_unchanged(&path, malformed, modified);
    let original = "model = 'old'\nscalar = 1\n";
    fs::write(&path, original).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    for value in [
        "[1,",
        "{ broken",
        "\"unterminated",
        "'unterminated",
        "123abc",
        "-1oops",
        "+2oops",
    ] {
        assert_eq!(
            set(&path, "model", value).unwrap_err().kind(),
            ErrorKind::InvalidInput,
            "{value}"
        );
        assert_unchanged(&path, original, modified);
    }
    for key in ["", "a..b", "\"unterminated"] {
        assert_eq!(
            get(&path, Some(key)).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            set(&path, key, "new").unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            unset(&path, key).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        set(&path, "scalar.child", "new").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    unset(&path, "scalar.child").unwrap();
    assert_unchanged(&path, original, modified);
    let absent = dir.path().join("missing/config.toml");
    assert!(set(&absent, "model", "[broken").is_err());
    assert!(!absent.parent().unwrap().exists());
}

#[test]
fn string_batches_are_forced_strings_and_fail_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    set_strings(&path, &[("absent", None)]).unwrap();
    assert!(!path.exists());
    set_strings(&path, &[("model", Some("true")), ("count", Some("42"))]).unwrap();
    assert_eq!(document(&path)["model"], toml::Value::from("true"));
    assert_eq!(document(&path)["count"], toml::Value::from("42"));
    let original = fs::read_to_string(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(
        set_strings(
            &path,
            &[("model", Some("changed")), ("count.child", Some("bad"))]
        )
        .is_err()
    );
    assert_unchanged(&path, &original, modified);
    set_strings(&path, &[("model", None), ("count", Some("false"))]).unwrap();
    assert!(document(&path).get("model").is_none());
    assert_eq!(document(&path)["count"], toml::Value::from("false"));
}

#[cfg(unix)]
#[test]
fn regular_and_dangling_symlinks_edit_targets_without_replacing_links() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    for dangling in [false, true] {
        let target = dir.path().join(if dangling {
            "new/target.toml"
        } else {
            "target.toml"
        });
        let link = dir.path().join(if dangling {
            "dangling.toml"
        } else {
            "link.toml"
        });
        let relative = target.strip_prefix(dir.path()).unwrap();
        symlink(relative, &link).unwrap();
        if dangling {
            assert_eq!(get(&link, None).unwrap_err().kind(), ErrorKind::NotFound);
            unset(&link, "model").unwrap();
            assert!(!target.exists());
        } else {
            fs::write(&target, "# keep\nmodel = 'old'\n").unwrap();
            assert_eq!(get(&link, Some("model")).unwrap(), "'old'");
        }
        set(&link, "model", "new/model").unwrap();
        assert_eq!(document(&target)["model"], toml::Value::from("new/model"));
        assert_eq!(
            get(&link, None).unwrap(),
            fs::read_to_string(&target).unwrap()
        );
        unset(&link, "model").unwrap();
        assert!(document(&target).get("model").is_none());
        assert_eq!(fs::read_link(&link).unwrap(), relative);
    }
}

#[cfg(unix)]
#[test]
fn parent_symlink_chains_and_dotdot_resolve_in_filesystem_order() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("real/child")).unwrap();
    symlink("real/child", dir.path().join("second")).unwrap();
    symlink("second", dir.path().join("first")).unwrap();
    let path = dir.path().join("first/../config.toml");
    let target = dir.path().join("real/config.toml");
    set(&path, "model", "chain/model").unwrap();
    assert_eq!(document(&target)["model"], toml::Value::from("chain/model"));
    assert!(!dir.path().join("config.toml").exists());
    assert_eq!(get(&path, Some("model")).unwrap(), "\"chain/model\"");
    unset(&path, "model").unwrap();
    assert!(document(&target).get("model").is_none());
    assert_eq!(
        fs::read_link(dir.path().join("first")).unwrap(),
        Path::new("second")
    );
    assert_eq!(
        fs::read_link(dir.path().join("second")).unwrap(),
        Path::new("real/child")
    );
}

#[test]
fn absent_unsets_preserve_crlf_bytes_and_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for bom in ["", "\u{feff}"] {
        let original = format!(
            "{bom}# heading\r\nmodel = 'old' # selected\r\ntext = '''first\r\nsecond'''\r\n[custom]\r\nvalue = 1\r\n"
        );
        fs::write(&path, &original).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        for key in ["absent", "absent.child", "custom.absent"] {
            unset(&path, key).unwrap();
            assert_unchanged(&path, &original, modified);
        }
        set_strings(&path, &[("absent", None), ("custom.absent", None)]).unwrap();
        assert_unchanged(&path, &original, modified);
        set_strings(&path, &[]).unwrap();
        assert_unchanged(&path, &original, modified);
    }
}

#[test]
fn keyed_values_round_trip_without_outer_comments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for value in [
        "true",
        "42",
        "1.25",
        "1979-05-27",
        "'old'",
        "\"# string content\"",
        "'''first\n# string content\nlast'''",
        "[1, # array comment\n 2]",
        "{ enabled = true, nested = { count = 2 } }",
    ] {
        let original = format!("# heading\nvalue = {value} # trailing comment\n");
        fs::write(&path, &original).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let expected = document(&path);
        let output = get(&path, Some("value")).unwrap();
        assert_eq!(output, value);
        assert_unchanged(&path, &original, modified);
        set(&path, "value", &output).unwrap();
        assert_eq!(document(&path), expected);
        assert_unchanged(&path, &original, modified);
    }
}

#[test]
fn keyed_table_output_remains_a_toml_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "[custom] # table comment\nvalue = true # value comment\n";
    fs::write(&path, original).unwrap();
    let output = get(&path, Some("custom")).unwrap();
    let parsed: toml::Value = toml::from_str(&output).unwrap();
    assert_eq!(parsed, document(&path)["custom"]);
    assert_eq!(get(&path, None).unwrap(), original);
}

#[test]
fn keyed_tables_return_the_complete_selected_subtree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for source in [
        "[selected]\nvalue = 1\n[selected.child]\nvalue = 2\n[selected.child.deep]\nvalue = 3\n[other]\nvalue = 4\n",
        "[selected.child.deep]\nvalue = 3\n[other]\nvalue = 4\n",
        "[selected]\nvalue = 1\n[[selected.children]]\nvalue = 2\n[selected.children.nested]\nvalue = 3\n[[selected.children]]\nvalue = 4\n[[selected.children.more]]\nvalue = 5\n[other]\nvalue = 6\n",
    ] {
        fs::write(&path, source).unwrap();
        let selected: toml::Value = toml::from_str(&get(&path, Some("selected")).unwrap()).unwrap();
        assert_eq!(selected, document(&path)["selected"], "{source}");
    }
}

#[test]
fn replacing_table_preserves_its_leading_and_header_comments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let source = "# leading\n[replace] # header\nold = 1\n[replace.child]\nold = 2\n\n# unrelated\n[keep] # keep header\nvalue = 3\n";
    fs::write(&path, source).unwrap();
    set(&path, "replace", "false").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "# leading\nreplace= false # header\n\n# unrelated\n[keep] # keep header\nvalue = 3\n"
    );
    assert_eq!(document(&path)["replace"], toml::Value::from(false));
}

#[test]
fn unset_through_non_tables_is_a_byte_preserving_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let source = "scalar = 1\r\narray = [1, 2]\r\n[[tables]]\r\nvalue = 3\r\n";
    fs::write(&path, source).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    for key in ["scalar.child", "array.child", "tables.child"] {
        unset(&path, key).unwrap();
        assert_eq!(
            set(&path, key, "4").unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_unchanged(&path, source, modified);
    }
}
