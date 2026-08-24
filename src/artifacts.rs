use std::path::{Path, PathBuf};

use tokio::fs::File;

pub(crate) fn directory(root: &Path, session_id: &str, call_id: &str) -> PathBuf {
    let root = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map_or_else(
            || root.join(".kit/artifacts"),
            |home| home.join(".kit/artifacts"),
        );
    root.join(safe_component(session_id))
        .join(safe_component(call_id))
}

pub(crate) async fn create(path: &Path) -> std::io::Result<(File, PathBuf)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
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
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate).await {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no available artifact filename under {}", parent.display()),
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
