use std::{
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use crate::resilient_fs as fs;

pub(crate) fn base(root: &Path) -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map_or_else(
            || root.join(".kit/artifacts"),
            |home| home.join(".kit/artifacts"),
        )
}

pub(crate) fn session_directory(base: &Path, session_id: &str) -> PathBuf {
    base.join(safe_component(session_id))
}

pub(crate) fn directory(root: &Path, session_id: &str, call_id: &str) -> PathBuf {
    session_directory(&base(root), session_id).join(safe_component(call_id))
}

/// Retain the complete artifact through the same filesystem as transcripts.
/// Call from a blocking task; a returned path is readable through ArtifactTool
/// even when the backing bytes have not yet reached disk.
pub(crate) fn write(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_private_dir_all(parent)?;
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("log");
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            path.to_path_buf()
        } else {
            path.with_file_name(format!("{stem}-{attempt}.{extension}"))
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .private(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no available artifact filename",
    ))
}

fn safe_component(value: &str) -> String {
    let prefix = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(48)
        .collect::<String>();
    let hash = blake3::hash(value.as_bytes()).to_hex().to_string();
    format!(
        "{}-{}",
        if prefix.is_empty() { "id" } else { &prefix },
        &hash[..12]
    )
}
