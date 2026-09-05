//! Symlink-compatible reads for user-selected configuration and context files.

use std::{io, path::Path};

// Resolve components in filesystem order. In particular, `..` applies to the
// resolved directory, not to the lexical parent of a directory symlink. Each
// lookup uses the overlay and never requires the final target to exist on disk.
fn resolve_in(
    filesystem: &crate::resilient_fs::Fs,
    path: &Path,
    links: &mut usize,
) -> io::Result<std::path::PathBuf> {
    use std::path::{Component, PathBuf};

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !filesystem.metadata(&resolved)?.is_dir() {
                    return Err(io::ErrorKind::NotADirectory.into());
                }
                // Popping a root has no effect, as with native path traversal.
                resolved.pop();
            }
            Component::Normal(name) => {
                if !filesystem.metadata(&resolved)?.is_dir() {
                    return Err(io::ErrorKind::NotADirectory.into());
                }
                resolved.push(name);
                if filesystem
                    .symlink_metadata(&resolved)?
                    .file_type()
                    .is_symlink()
                {
                    if *links == 40 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "too many symbolic links in configuration path",
                        ));
                    }
                    *links += 1;
                    let target = filesystem.read_link(&resolved)?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        resolved
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(target)
                    };
                    resolved = resolve_in(filesystem, &target, links)?;
                }
            }
        }
    }
    Ok(resolved)
}

pub fn read_in(filesystem: &crate::resilient_fs::Fs, path: &Path) -> io::Result<Vec<u8>> {
    filesystem.read(resolve_in(filesystem, path, &mut 0)?)
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_in(crate::resilient_fs::global(), path)
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
