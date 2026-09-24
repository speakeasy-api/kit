// Production policy is deliberately non-overridable. Test-only scopes below
// retain assertions and unwrap ergonomics; placeholder lints remain denied.
#![cfg_attr(
    not(test),
    forbid(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::disallowed_methods,
        clippy::disallowed_macros
    )
)]

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

fn main() -> io::Result<()> {
    let docs_root = Path::new("docs/user");
    println!("cargo:rerun-if-changed={}", docs_root.display());

    let mut paths = Vec::new();
    collect_markdown(docs_root, &mut paths).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not collect {}: {error}", docs_root.display()),
        )
    })?;
    paths.sort();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} must contain bundled Markdown documentation",
                docs_root.display()
            ),
        ));
    }

    let mut generated = String::from("&[\n");
    for path in paths {
        let content = fs::read_to_string(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not read {}: {error}", path.display()),
            )
        })?;
        generated.push_str(&format!("    ({:?}, {:?}),\n", slash_path(&path), content));
    }
    generated.push(']');

    let output = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OUT_DIR is not set"))?,
    )
    .join("bundled_docs.rs");
    fs::write(&output, generated).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not write {}: {error}", output.display()),
        )
    })
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
