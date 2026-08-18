use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

fn main() {
    let docs_root = Path::new("docs/user");
    println!("cargo:rerun-if-changed={}", docs_root.display());

    let mut paths = Vec::new();
    collect_markdown(docs_root, &mut paths)
        .unwrap_or_else(|error| panic!("could not collect {}: {error}", docs_root.display()));
    paths.sort();
    assert!(
        !paths.is_empty(),
        "{} must contain bundled Markdown documentation",
        docs_root.display()
    );

    let mut generated = String::from("&[\n");
    for path in paths {
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
        generated.push_str(&format!("    ({:?}, {:?}),\n", slash_path(&path), content));
    }
    generated.push(']');

    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("bundled_docs.rs");
    fs::write(output, generated).expect("write bundled documentation");
}

fn collect_markdown(directory: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "documentation path must not be a symlink: {}",
                    path.display()
                ),
            ));
        }
        if file_type.is_dir() {
            collect_markdown(&path, paths)?;
        } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "md")
        {
            paths.push(path);
        }
    }
    Ok(())
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
