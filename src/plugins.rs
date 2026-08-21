use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use agentkit_plugins::{AgentPlugin, PluginMcpServer};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::{Host, Url};

const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TAR_STREAM_BYTES: u64 = MAX_EXPANDED_BYTES + 16 * 1024 * 1024;

struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin tar stream exceeds expanded size limit",
            ));
        }
        let limit = usize::try_from(self.remaining.min(buffer.len() as u64)).unwrap();
        let read = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum PluginConfig {
    Path {
        path: PathBuf,
    },
    Archive {
        url: String,
        sha256: String,
        subdir: Option<PathBuf>,
    },
}

#[derive(Clone, Debug)]
pub struct ResolvedPluginMcp {
    pub(crate) alias: String,
    pub(crate) manifest_name: String,
    pub(crate) root: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) servers: Vec<PluginMcpServer>,
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedPlugins {
    pub package_roots: Vec<PathBuf>,
    pub skill_directories: Vec<PathBuf>,
    pub mcp_plugins: Vec<ResolvedPluginMcp>,
}

pub async fn resolve(
    configs: &BTreeMap<String, PluginConfig>,
    runtime_root: &Path,
    cache_root: &Path,
    data_root: &Path,
) -> Result<ResolvedPlugins, String> {
    let configs = configs.clone();
    let runtime_root = runtime_root.to_path_buf();
    let cache_root = cache_root.to_path_buf();
    let data_root = data_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        resolve_blocking(&configs, &runtime_root, &cache_root, &data_root)
    })
    .await
    .map_err(|error| format!("plugin resolver task failed: {error}"))?
}

fn resolve_blocking(
    configs: &BTreeMap<String, PluginConfig>,
    runtime_root: &Path,
    cache_root: &Path,
    data_root: &Path,
) -> Result<ResolvedPlugins, String> {
    let mut resolved = ResolvedPlugins::default();
    let mut manifest_names = BTreeSet::new();
    for (alias, config) in configs {
        validate_alias(alias)?;
        let root = match config {
            PluginConfig::Path { path } => resolve_path(path, runtime_root)?,
            PluginConfig::Archive {
                url,
                sha256,
                subdir,
            } => resolve_archive(url, sha256, subdir.as_deref(), cache_root)?,
        };
        let plugin = AgentPlugin::load(&root).map_err(|error| {
            format!(
                "could not load plugin {alias:?} from {}: {error}",
                root.display()
            )
        })?;
        if !manifest_names.insert(plugin.manifest().name.clone()) {
            return Err(format!(
                "plugin {alias:?} duplicates manifest name {:?}",
                plugin.manifest().name
            ));
        }
        for diagnostic in plugin.diagnostics() {
            let path = diagnostic
                .path
                .as_deref()
                .map(|path| format!(" at {}", path.display()))
                .unwrap_or_default();
            eprintln!(
                "plugin {alias} ({:?}){path}: {}",
                diagnostic.kind, diagnostic.message
            );
        }
        if !plugin.mcp_servers().is_empty() {
            let data_dir = data_root.join(&plugin.manifest().name);
            fs::create_dir_all(&data_dir).map_err(|error| {
                format!(
                    "could not create data directory for plugin {alias:?} at {}: {error}",
                    data_dir.display()
                )
            })?;
            let data_dir = data_dir.canonicalize().map_err(|error| {
                format!("could not resolve data directory for plugin {alias:?}: {error}")
            })?;
            if !data_dir.is_dir() {
                return Err(format!(
                    "plugin data path is not a directory: {}",
                    data_dir.display()
                ));
            }
            resolved.mcp_plugins.push(ResolvedPluginMcp {
                alias: alias.clone(),
                manifest_name: plugin.manifest().name.clone(),
                root: plugin.root().to_path_buf(),
                data_dir,
                servers: plugin.mcp_servers().to_vec(),
            });
        }
        resolved.package_roots.push(plugin.root().to_path_buf());
        resolved
            .skill_directories
            .extend(plugin.skill_directories());
    }
    Ok(resolved)
}

fn validate_alias(alias: &str) -> Result<(), String> {
    let bytes = alias.as_bytes();
    if alias.is_empty()
        || alias.len() > 64
        || alias.contains("--")
        || !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(format!(
            "invalid plugin alias {alias:?}; use 1-64 lowercase alphanumeric or hyphen characters"
        ));
    }
    Ok(())
}

fn resolve_path(path: &Path, runtime_root: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        runtime_root.join(path)
    };
    path.canonicalize()
        .map_err(|error| format!("could not resolve plugin path {}: {error}", path.display()))
}

fn resolve_archive(
    value: &str,
    expected_digest: &str,
    subdir: Option<&Path>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let url = Url::parse(value).map_err(|error| format!("invalid plugin archive URL: {error}"))?;
    validate_download_url(&url)?;
    let expected = parse_sha256(expected_digest)?;
    let subdir = subdir.map(validate_relative_path).transpose()?;
    let digest = expected_digest.to_ascii_lowercase();
    fs::create_dir_all(cache_root).map_err(|error| {
        format!(
            "could not create plugin cache {}: {error}",
            cache_root.display()
        )
    })?;
    let destination = cache_root.join(&digest);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(format!(
                "plugin cache entry is not a directory: {}",
                destination.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not inspect plugin cache entry: {error}")),
    }
    if !destination.exists() {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|error| error.to_string())?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let staging = cache_root.join(format!(".{digest}.staging-{suffix}"));
        fs::create_dir(&staging)
            .map_err(|error| format!("could not create plugin staging directory: {error}"))?;
        let result = (|| {
            let bytes = download(&url)?;
            let actual = Sha256::digest(&bytes);
            if actual.as_slice() != expected {
                return Err(format!("SHA-256 mismatch for plugin archive {url}"));
            }
            extract_archive(&bytes, &staging)?;
            let candidate = select_package_root(&staging, subdir.as_deref())?;
            AgentPlugin::load(&candidate).map_err(|error| {
                format!(
                    "invalid plugin archive package at {}: {error}",
                    candidate.display()
                )
            })?;
            match fs::rename(&staging, &destination) {
                Ok(()) => Ok(()),
                Err(_error) if destination.is_dir() => Ok(()),
                Err(error) => Err(format!("could not publish plugin archive: {error}")),
            }
        })();
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
    }
    select_package_root(&destination, subdir.as_deref())
}

fn validate_download_url(url: &Url) -> Result<(), String> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("plugin archive URL must not contain credentials or a fragment".into());
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host == "localhost",
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err("plugin archive URL must use HTTPS (or loopback HTTP)".into());
    }
    Ok(())
}

fn parse_sha256(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("plugin archive sha256 must contain exactly 64 hexadecimal characters".into());
    }
    let mut digest = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).expect("hexadecimal is UTF-8");
        digest[index] = u8::from_str_radix(text, 16).expect("hexadecimal pair parses");
    }
    Ok(digest)
}

fn download(url: &Url) -> Result<Vec<u8>, String> {
    let allow_loopback_http = url.scheme() == "http";
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many plugin archive redirects");
            }
            let target = attempt.url();
            if validate_download_url(target).is_ok()
                && (target.scheme() == "https" || allow_loopback_http)
            {
                attempt.follow()
            } else {
                attempt.error("plugin archive redirect is not secure")
            }
        }))
        .build()
        .map_err(|error| format!("could not create plugin HTTP client: {error}"))?;
    let response = client
        .get(url.clone())
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("could not download plugin archive {url}: {error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES)
    {
        return Err("plugin archive exceeds the 64 MiB download limit".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read plugin archive: {error}"))?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err("plugin archive exceeds the 64 MiB download limit".into());
    }
    Ok(bytes)
}

fn extract_archive(bytes: &[u8], destination: &Path) -> Result<(), String> {
    if bytes.starts_with(b"PK\x03\x04") {
        extract_zip(bytes, destination)
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        extract_tar(
            LimitedReader {
                inner: GzDecoder::new(Cursor::new(bytes)),
                remaining: MAX_TAR_STREAM_BYTES,
            },
            destination,
        )
    } else {
        extract_tar(
            LimitedReader {
                inner: Cursor::new(bytes),
                remaining: MAX_TAR_STREAM_BYTES,
            },
            destination,
        )
    }
}

fn extract_zip(bytes: &[u8], destination: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("invalid ZIP plugin archive: {error}"))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err("plugin archive contains too many entries".into());
    }
    let mut paths = BTreeSet::new();
    let mut expanded = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let path = validate_archive_path(Path::new(entry.name()))?;
        if !paths.insert(path.clone()) {
            return Err(format!("duplicate plugin archive path {}", path.display()));
        }
        let output = destination.join(&path);
        let mode = entry.unix_mode().unwrap_or(0);
        let file_type = mode & 0o170000;
        if entry.is_dir() {
            fs::create_dir_all(&output).map_err(|error| error.to_string())?;
        } else if file_type == 0 || file_type == 0o100000 {
            expanded = expanded
                .checked_add(entry.size())
                .ok_or("plugin archive expanded size overflow")?;
            if entry.size() > MAX_FILE_BYTES || expanded > MAX_EXPANDED_BYTES {
                return Err("plugin archive exceeds expanded size limits".into());
            }
            let size = entry.size();
            write_entry(&mut entry, &output, size)?;
        } else {
            return Err(format!("unsupported ZIP entry type at {}", path.display()));
        }
    }
    Ok(())
}

fn extract_tar(reader: impl Read, destination: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let mut paths = BTreeSet::new();
    let mut count = 0usize;
    let mut expanded = 0u64;
    for entry in archive
        .entries()
        .map_err(|error| format!("invalid tar archive: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("invalid tar entry: {error}"))?;
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err("plugin archive contains too many entries".into());
        }
        let path = validate_archive_path(&entry.path().map_err(|error| error.to_string())?)?;
        if !paths.insert(path.clone()) {
            return Err(format!("duplicate plugin archive path {}", path.display()));
        }
        let output = destination.join(&path);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&output).map_err(|error| error.to_string())?;
        } else if kind.is_file() {
            let size = entry.size();
            expanded = expanded
                .checked_add(size)
                .ok_or("plugin archive expanded size overflow")?;
            if size > MAX_FILE_BYTES || expanded > MAX_EXPANDED_BYTES {
                return Err("plugin archive exceeds expanded size limits".into());
            }
            write_entry(&mut entry, &output, size)?;
        } else {
            return Err(format!("unsupported tar entry type at {}", path.display()));
        }
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() || path.to_string_lossy().contains('\\') {
        return Err("plugin archive contains an invalid path".into());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "plugin archive path escapes extraction root: {}",
                    path.display()
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err("plugin archive contains an empty path".into());
    }
    Ok(normalized)
}

fn validate_relative_path(path: &Path) -> Result<PathBuf, String> {
    validate_archive_path(path)
}

fn write_entry(reader: &mut impl Read, output: &Path, declared_size: u64) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| {
            format!(
                "could not create extracted file {}: {error}",
                output.display()
            )
        })?;
    let copied = io::copy(&mut reader.take(MAX_FILE_BYTES + 1), &mut file)
        .map_err(|error| format!("could not extract {}: {error}", output.display()))?;
    if copied != declared_size || copied > MAX_FILE_BYTES {
        return Err(format!("invalid extracted size for {}", output.display()));
    }
    file.flush().map_err(|error| error.to_string())
}

fn select_package_root(extraction: &Path, subdir: Option<&Path>) -> Result<PathBuf, String> {
    let base = if extraction.join("plugin.json").is_file() {
        extraction.to_path_buf()
    } else {
        let entries = fs::read_dir(extraction)
            .map_err(|error| format!("could not inspect extracted plugin: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        if entries.len() != 1 || !entries[0].path().is_dir() {
            return Err(
                "plugin archive must contain plugin.json or one top-level directory".into(),
            );
        }
        entries[0].path()
    };
    let selected = subdir.map_or(base.clone(), |subdir| base.join(subdir));
    let canonical_base = extraction
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let selected = selected.canonicalize().map_err(|error| {
        format!(
            "could not resolve plugin archive package {}: {error}",
            selected.display()
        )
    })?;
    if !selected.is_dir() || !selected.starts_with(&canonical_base) {
        return Err("plugin archive subdir escapes the extracted package".into());
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    const MANIFEST: &str = r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"test-plugin"}"#;

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn serve_once(body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        (format!("http://{address}/plugin.tar"), handle)
    }

    #[test]
    fn parses_strict_plugin_sources() {
        let path: PluginConfig = toml::from_str("source = 'path'\npath = './plugin'").unwrap();
        assert!(matches!(path, PluginConfig::Path { .. }));
        let archive: PluginConfig = toml::from_str(&format!(
            "source = 'archive'\nurl = 'https://example.com/plugin.zip'\nsha256 = '{}'",
            "ab".repeat(32)
        ))
        .unwrap();
        assert!(matches!(archive, PluginConfig::Archive { .. }));
        assert!(
            toml::from_str::<PluginConfig>(
                "source = 'path'\npath = '.'\nurl = 'https://example.com'"
            )
            .is_err()
        );
    }

    #[test]
    fn validates_aliases_digests_and_paths() {
        assert!(validate_alias("review-tools").is_ok());
        assert!(validate_alias("Bad_Name").is_err());
        assert!(parse_sha256(&"01".repeat(32)).is_ok());
        assert!(parse_sha256("01").is_err());
        assert!(validate_archive_path(Path::new("skills/review/SKILL.md")).is_ok());
        assert!(validate_archive_path(Path::new("../escape")).is_err());
        assert!(validate_archive_path(Path::new("windows\\escape")).is_err());
    }

    fn tar_with_file(path: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, body).unwrap();
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn extracts_tar_and_tar_gz() {
        let bytes = tar_with_file("plugin/plugin.json", b"{}");
        let tar_destination = tempfile::tempdir().unwrap();
        extract_archive(&bytes, tar_destination.path()).unwrap();
        assert!(tar_destination.path().join("plugin/plugin.json").is_file());

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), Default::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let gzip_destination = tempfile::tempdir().unwrap();
        extract_archive(&compressed, gzip_destination.path()).unwrap();
        assert!(gzip_destination.path().join("plugin/plugin.json").is_file());
    }

    #[test]
    fn extracts_zip_and_rejects_duplicate_paths() {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default();
            archive.start_file("plugin/plugin.json", options).unwrap();
            archive.write_all(b"{}").unwrap();
            archive.finish().unwrap();
        }
        let destination = tempfile::tempdir().unwrap();
        extract_archive(bytes.get_ref(), destination.path()).unwrap();
        assert!(destination.path().join("plugin/plugin.json").is_file());

        let mut duplicate = Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut duplicate);
            let options = zip::write::SimpleFileOptions::default();
            archive.start_file("a/./file", options).unwrap();
            archive.write_all(b"one").unwrap();
            archive.start_file("a/file", options).unwrap();
            archive.write_all(b"two").unwrap();
            archive.finish().unwrap();
        }
        let destination = tempfile::tempdir().unwrap();
        assert!(extract_archive(duplicate.get_ref(), destination.path()).is_err());
    }

    #[test]
    fn rejects_tar_links() {
        for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let mut bytes = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut bytes);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(kind);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_link_name("target").unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, "plugin/link", io::empty())
                    .unwrap();
                builder.finish().unwrap();
            }
            let destination = tempfile::tempdir().unwrap();
            assert!(extract_archive(&bytes, destination.path()).is_err());
        }
    }

    #[test]
    fn archive_resolution_checks_sha_and_reuses_cache() {
        let bytes = tar_with_file("plugin/plugin.json", MANIFEST.as_bytes());
        let digest = sha256_hex(&bytes);
        let cache = tempfile::tempdir().unwrap();
        let (url, server) = serve_once(bytes.clone());
        let root = resolve_archive(&url, &digest, None, cache.path()).unwrap();
        server.join().unwrap();
        assert!(root.join("plugin.json").is_file());

        // A cache hit does not contact the now-closed one-shot server.
        assert_eq!(
            resolve_archive(&url, &digest, None, cache.path()).unwrap(),
            root
        );

        let other_cache = tempfile::tempdir().unwrap();
        let (url, server) = serve_once(bytes);
        let error = resolve_archive(&url, &"00".repeat(32), None, other_cache.path()).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("SHA-256 mismatch"));
    }

    #[test]
    fn rejects_invalid_archive_manifest() {
        let bytes = tar_with_file("plugin/plugin.json", b"{}");
        let digest = sha256_hex(&bytes);
        let cache = tempfile::tempdir().unwrap();
        let (url, server) = serve_once(bytes);
        let error = resolve_archive(&url, &digest, None, cache.path()).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("invalid plugin archive package"));
    }

    #[test]
    fn selects_single_top_level_directory_and_subdir() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("repository/plugins/reviewer")).unwrap();
        fs::write(
            temp.path().join("repository/plugins/reviewer/plugin.json"),
            "{}",
        )
        .unwrap();
        assert_eq!(
            select_package_root(temp.path(), Some(Path::new("plugins/reviewer"))).unwrap(),
            temp.path()
                .join("repository/plugins/reviewer")
                .canonicalize()
                .unwrap()
        );
    }
}
