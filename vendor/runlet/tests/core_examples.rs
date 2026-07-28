use runlet::Runtime;
use std::{fs, path::PathBuf};

#[test]
fn core_examples_compile_and_run_without_tools() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut files = fs::read_dir(examples)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "rnlt")
        })
        .collect::<Vec<_>>();
    files.sort();

    assert!(!files.is_empty());
    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        let compiled = runtime
            .compile(&source)
            .unwrap_or_else(|diagnostics| panic!("{}: {diagnostics:#?}", file.display()));
        runtime
            .run(&compiled)
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
    }
}
