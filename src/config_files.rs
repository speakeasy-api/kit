//! Symlink-compatible reads for user-selected configuration and context files.

use std::{io, path::Path};

// User-selected configuration files may be symlinks. Resolve only at this read
// boundary: managed storage continues to reject final symlinks. Resolve each
// target through the facade so even a memory-only target remains readable.
pub fn read_in(filesystem: &crate::resilient_fs::Fs, path: &Path) -> std::io::Result<Vec<u8>> {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        if !filesystem.symlink_metadata(&path)?.file_type().is_symlink() {
            return filesystem.read(path);
        }
        let target = filesystem.read_link(&path)?;
        path = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or_else(|| Path::new(".")).join(target)
        };
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "too many symbolic links in configuration path",
    ))
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_in(crate::resilient_fs::global(), path)
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
