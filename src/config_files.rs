//! Symlink-compatible reads for user-selected configuration and context files.

use std::{io, path::Path};

// Resolve components in filesystem order. In particular, `..` applies to the
// resolved directory, not to the lexical parent of a directory symlink. Each
// lookup uses the overlay and never requires the final target to exist on disk.
fn resolve_in(
    filesystem: &crate::resilient_fs::Fs,
    path: &Path,
    links: &mut usize,
    allow_missing: bool,
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
                if !is_directory(filesystem, &resolved, false)? {
                    return Err(io::ErrorKind::NotADirectory.into());
                }
                // Popping a root has no effect, as with native path traversal.
                resolved.pop();
            }
            Component::Normal(name) => {
                if !is_directory(filesystem, &resolved, allow_missing)? {
                    return Err(io::ErrorKind::NotADirectory.into());
                }
                resolved.push(name);
                let metadata = match filesystem.symlink_metadata(&resolved) {
                    Ok(metadata) => Some(metadata),
                    Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                };
                if metadata.is_some_and(|metadata| metadata.file_type().is_symlink()) {
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
                    resolved = resolve_in(filesystem, &target, links, allow_missing)?;
                }
            }
        }
    }
    Ok(resolved)
}

pub fn read_in(filesystem: &crate::resilient_fs::Fs, path: &Path) -> io::Result<Vec<u8>> {
    filesystem.read(resolve_in(filesystem, path, &mut 0, false)?)
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_in(crate::resilient_fs::global(), path)
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn is_directory(
    filesystem: &crate::resilient_fs::Fs,
    path: &Path,
    allow_missing: bool,
) -> io::Result<bool> {
    match filesystem.metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Resolve links before replacing a config, allowing a new file or dangling target.
pub(crate) fn resolve_for_write(path: &Path) -> io::Result<std::path::PathBuf> {
    resolve_in(crate::resilient_fs::global(), path, &mut 0, true)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::resolve_for_write;
    use std::{fs, io::ErrorKind};

    #[test]
    fn write_resolution_rejects_missing_directory_before_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/../config.toml");

        assert_eq!(
            resolve_for_write(&path).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert!(!dir.path().join("missing").exists());
        assert!(!dir.path().join("config.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn set_through_missing_directory_before_parent_does_not_write_destination() {
        use std::{os::unix::fs::symlink, path::Path};

        for existing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let destination = dir.path().join("config.toml");
            let original = "model = 'old'\n";
            if existing {
                fs::write(&destination, original).unwrap();
            }
            let link = dir.path().join("link.toml");
            let target = Path::new("missing/../config.toml");
            symlink(target, &link).unwrap();

            assert_eq!(
                crate::config_editor::set(&link, "model", "new")
                    .unwrap_err()
                    .kind(),
                ErrorKind::NotFound
            );
            if existing {
                assert_eq!(fs::read_to_string(&destination).unwrap(), original);
            } else {
                assert!(!destination.exists());
            }
            assert!(!dir.path().join("missing").exists());
            assert_eq!(fs::read_link(&link).unwrap(), target);
        }
    }
}
