use serde_json::Value;
use std::{fs, path::Path};

fn read_json(path: impl AsRef<Path>) -> Value {
    let path = path.as_ref();
    let source = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    serde_json::from_str(&source)
        .unwrap_or_else(|error| panic!("invalid JSON in {}: {error}", path.display()))
}

#[test]
fn vscode_extension_registers_the_complete_runlet_grammar() {
    let package = read_json("editors/vscode/package.json");
    assert_eq!(
        package["contributes"]["languages"][0]["extensions"][0],
        ".rnlt"
    );
    assert_eq!(
        package["contributes"]["grammars"][0]["scopeName"],
        "source.runlet"
    );

    let grammar = read_json("editors/vscode/syntaxes/runlet.tmLanguage.json");
    assert_eq!(grammar["scopeName"], "source.runlet");

    let repository = grammar["repository"]
        .as_object()
        .expect("grammar repository must be an object");
    for group in [
        "comments",
        "strings",
        "numbers",
        "constants",
        "keywords",
        "word-operators",
        "bindings",
        "calls",
        "properties",
        "operators",
        "punctuation",
    ] {
        assert!(repository.contains_key(group), "missing `{group}` grammar");
    }

    let configuration = read_json("editors/vscode/language-configuration.json");
    assert_eq!(configuration["comments"]["lineComment"], "//");
}

#[test]
fn zed_and_vim_support_runlet_files() {
    let extension = fs::read_to_string("editors/zed/extension.toml").unwrap();
    assert!(extension.contains("repository = \"https://github.com/danielkov/runlet\""));
    let revision = extension
        .lines()
        .find_map(|line| line.strip_prefix("rev = \"")?.strip_suffix('"'))
        .expect("Zed grammar revision");
    assert_eq!(revision.len(), 40);
    assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let zed = fs::read_to_string("editors/zed/languages/runlet/config.toml").unwrap();
    assert!(zed.contains("grammar = \"runlet\""));
    assert!(zed.contains("path_suffixes = [\"rnlt\"]"));

    let highlights = fs::read_to_string("editors/zed/languages/runlet/highlights.scm").unwrap();
    for capture in ["@comment", "@string", "@number", "@keyword", "@function"] {
        assert!(
            highlights.contains(capture),
            "missing Zed capture {capture}"
        );
    }

    let vim = fs::read_to_string("editors/vim/syntax/runlet.vim").unwrap();
    for group in [
        "runletComment",
        "runletString",
        "runletKeyword",
        "runletFunction",
    ] {
        assert!(vim.contains(group), "missing Vim syntax group {group}");
    }

    let detection = fs::read_to_string("editors/vim/ftdetect/runlet.vim").unwrap();
    assert!(detection.contains("*.rnlt setfiletype runlet"));
}
