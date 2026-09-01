use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, RwLock, mpsc},
    thread,
    time::{Duration, Instant, SystemTime},
};

use agentkit_plugins::{AgentPlugin, PluginDiagnosticKind, PluginMcpServer};
use agentkit_tool_skills::{Skill, SkillRegistry};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::process_tree::{isolate_process_tree, terminate_process_tree_with_pid};

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TAR_STREAM_BYTES: u64 = MAX_EXPANDED_BYTES + 16 * 1024 * 1024;
const MAX_GIT_TREE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_GIT_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_GIT_DIAGNOSTIC_BYTES: u64 = 64 * 1024;
const MAX_GIT_OBJECT_ENTRIES: usize = MAX_ARCHIVE_ENTRIES * 10;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_OBJECT_SCAN_INITIAL_DELAY: Duration = Duration::from_millis(100);
const GIT_OBJECT_SCAN_MAX_DELAY: Duration = Duration::from_secs(1);
const STALE_STAGING_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const GIT_CACHE_VERSION: &str = "git-v1";
const GIT_PRIVATE_FETCH_REF: &str = "refs/kit/plugin-source";

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
        let limit = usize::try_from(self.remaining.min(buffer.len() as u64)).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "plugin read limit is too large")
        })?;
        let read = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum PluginConfig {
    Path {
        path: PathBuf,
    },
    Archive {
        url: String,
        sha256: String,
        subdir: Option<PathBuf>,
    },
    Git {
        url: String,
        rev: Option<String>,
        subdir: Option<String>,
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
    pub skills: Vec<Skill>,
    pub mcp_plugins: Vec<ResolvedPluginMcp>,
}

#[derive(Deserialize)]
struct PluginConfigFile {
    #[serde(default)]
    plugins: BTreeMap<String, PluginConfig>,
}

#[derive(Clone)]
pub struct PluginRuntime {
    inner: Arc<PluginRuntimeInner>,
}

struct PluginRuntimeInner {
    config_path: PathBuf,
    runtime_root: PathBuf,
    cache_root: PathBuf,
    skill_cache_root: PathBuf,
    data_root: PathBuf,
    published: RwLock<Arc<ResolvedPlugins>>,
    generation_barrier: Arc<tokio::sync::RwLock<()>>,
}

#[derive(Debug)]
pub(crate) struct StagedPlugins {
    pub(crate) resolved: ResolvedPlugins,
}

impl PluginRuntime {
    pub async fn load(
        config_path: PathBuf,
        runtime_root: PathBuf,
        cache_root: PathBuf,
        data_root: PathBuf,
    ) -> Result<Self, String> {
        let runtime = Self::new(
            config_path,
            runtime_root,
            cache_root,
            data_root,
            ResolvedPlugins::default(),
        );
        let staged = runtime.stage().await?;
        runtime.publish(staged.resolved);
        Ok(runtime)
    }

    pub fn new(
        config_path: PathBuf,
        runtime_root: PathBuf,
        cache_root: PathBuf,
        data_root: PathBuf,
        initial: ResolvedPlugins,
    ) -> Self {
        static NEXT_RUNTIME_CACHE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let cache_id = NEXT_RUNTIME_CACHE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut random = [0_u8; 16];
        let suffix = if getrandom::fill(&mut random).is_ok() {
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        } else {
            format!(
                "{}-{cache_id}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_nanos())
            )
        };
        let skill_cache_root = cache_root.join("runtime-skill-generations").join(suffix);
        Self {
            inner: Arc::new(PluginRuntimeInner {
                config_path,
                runtime_root,
                cache_root,
                skill_cache_root,
                data_root,
                published: RwLock::new(Arc::new(initial)),
                generation_barrier: Arc::new(tokio::sync::RwLock::new(())),
            }),
        }
    }

    pub fn snapshot(&self) -> Arc<ResolvedPlugins> {
        self.inner
            .published
            .read()
            .expect("plugin generation lock poisoned")
            .clone()
    }

    pub(crate) async fn stage(&self) -> Result<StagedPlugins, String> {
        let contents = match tokio::fs::read_to_string(&self.inner.config_path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(bounded_diagnostic(format!(
                    "could not read plugin config {}: {error}",
                    self.inner.config_path.display()
                )));
            }
        };
        let configs = if contents.is_empty() {
            BTreeMap::new()
        } else {
            toml::from_str::<PluginConfigFile>(&contents)
                .map_err(|error| {
                    bounded_diagnostic(format!(
                        "invalid plugin config {}: {error}",
                        self.inner.config_path.display()
                    ))
                })?
                .plugins
        };
        cleanup_runtime_skill_generations(&self.inner.skill_cache_root, &self.snapshot());
        let resolved = resolve_with_skill_cache(
            &configs,
            &self.inner.runtime_root,
            &self.inner.cache_root,
            &self.inner.skill_cache_root,
            &self.inner.data_root,
        )
        .await
        .map_err(bounded_diagnostic)?;
        Ok(StagedPlugins { resolved })
    }

    pub(crate) async fn generation_lease(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.inner.generation_barrier.clone().read_owned().await
    }

    pub(crate) fn try_generation_lease(&self) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.inner.generation_barrier.clone().try_read_owned().ok()
    }

    pub(crate) async fn generation_writer(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.inner.generation_barrier.clone().write_owned().await
    }

    pub(crate) fn publish(&self, resolved: ResolvedPlugins) {
        let mut published = self
            .inner
            .published
            .write()
            .expect("plugin generation lock poisoned");
        if resolved_generation_key(&published) != resolved_generation_key(&resolved) {
            *published = Arc::new(resolved);
        }
        let current = published.clone();
        drop(published);
        cleanup_runtime_skill_generations(&self.inner.skill_cache_root, &current);
    }
}

impl Drop for PluginRuntimeInner {
    fn drop(&mut self) {
        make_tree_writable(&self.skill_cache_root);
        let _ = fs::remove_dir_all(&self.skill_cache_root);
    }
}

fn resolved_generation_key(resolved: &ResolvedPlugins) -> blake3::Hash {
    let mut key = blake3::Hasher::new();
    for root in &resolved.package_roots {
        key.update(root.as_os_str().as_encoded_bytes());
        key.update(&[0]);
    }
    for directory in &resolved.skill_directories {
        key.update(directory.as_os_str().as_encoded_bytes());
        key.update(&[0]);
    }
    key.update(format!("{:?}", resolved.mcp_plugins).as_bytes());
    key.finalize()
}

pub(crate) fn bounded_diagnostic(mut message: String) -> String {
    const LIMIT: usize = 2_048;
    const ELLIPSIS: &str = "...";
    if message.len() > LIMIT {
        let mut end = LIMIT - ELLIPSIS.len();
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str(ELLIPSIS);
    }
    message
}

pub async fn resolve(
    configs: &BTreeMap<String, PluginConfig>,
    runtime_root: &Path,
    cache_root: &Path,
    data_root: &Path,
) -> Result<ResolvedPlugins, String> {
    resolve_with_skill_cache(configs, runtime_root, cache_root, cache_root, data_root).await
}

async fn resolve_with_skill_cache(
    configs: &BTreeMap<String, PluginConfig>,
    runtime_root: &Path,
    cache_root: &Path,
    skill_cache_root: &Path,
    data_root: &Path,
) -> Result<ResolvedPlugins, String> {
    let configs = configs.clone();
    let runtime_root = runtime_root.to_path_buf();
    let cache_root = cache_root.to_path_buf();
    let skill_cache_root = skill_cache_root.to_path_buf();
    let data_root = data_root.to_path_buf();
    let mut resolved = tokio::task::spawn_blocking(move || {
        resolve_blocking(
            &configs,
            &runtime_root,
            &cache_root,
            &skill_cache_root,
            &data_root,
        )
    })
    .await
    .map_err(|error| format!("plugin resolver task failed: {error}"))??;
    let registry = SkillRegistry::from_skill_dirs(resolved.skill_directories.clone())
        .discover_skills()
        .await;
    resolved.skills = registry.skills().into_iter().cloned().collect();
    Ok(resolved)
}

fn resolve_blocking(
    configs: &BTreeMap<String, PluginConfig>,
    runtime_root: &Path,
    cache_root: &Path,
    skill_cache_root: &Path,
    data_root: &Path,
) -> Result<ResolvedPlugins, String> {
    let mut resolved = ResolvedPlugins::default();
    let mut manifest_names = BTreeSet::new();
    let mut loaded_generations = Vec::new();
    for (alias, config) in configs {
        validate_alias(alias)?;
        let root = match config {
            PluginConfig::Path { path } => resolve_path(path, runtime_root)?,
            PluginConfig::Archive {
                url,
                sha256,
                subdir,
            } => resolve_archive(url, sha256, subdir.as_deref(), cache_root)?,
            PluginConfig::Git { url, rev, subdir } => {
                resolve_git(url, rev.as_deref(), subdir.as_deref(), cache_root)?
            }
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
        validate_plugin_diagnostics(alias, &plugin)?;
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
        loaded_generations.push((
            alias.clone(),
            plugin.root().to_path_buf(),
            plugin_semantic_key(&plugin),
        ));
    }
    let (package_roots, skill_directories) = snapshot_skills(
        &resolved.package_roots,
        &resolved.skill_directories,
        skill_cache_root,
    )?;
    for (alias, root, expected) in loaded_generations {
        let plugin = AgentPlugin::load(&root).map_err(|error| {
            format!(
                "could not revalidate plugin {alias:?} from {}: {error}",
                root.display()
            )
        })?;
        validate_plugin_diagnostics(&alias, &plugin)?;
        if plugin_semantic_key(&plugin) != expected {
            return Err(format!(
                "plugin {alias:?} changed while its generation was being staged"
            ));
        }
    }
    resolved.package_roots = package_roots;
    resolved.skill_directories = skill_directories;
    Ok(resolved)
}

fn plugin_semantic_key(plugin: &AgentPlugin) -> blake3::Hash {
    let mut key = blake3::Hasher::new();
    key.update(plugin.manifest().name.as_bytes());
    key.update(format!("{:?}", plugin.mcp_servers()).as_bytes());
    for skill in plugin.skill_directories() {
        key.update(skill.as_os_str().as_encoded_bytes());
        key.update(&[0]);
    }
    key.finalize()
}

#[derive(Debug, PartialEq, Eq)]
enum CachedEntry {
    Directory,
    File(Vec<u8>),
}

fn snapshot_skills(
    package_roots: &[PathBuf],
    skill_directories: &[PathBuf],
    cache_root: &Path,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    if skill_directories.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut inventory = BTreeMap::<PathBuf, CachedEntry>::new();
    let mut captured_bytes = 0_u64;
    for (index, package) in package_roots.iter().enumerate() {
        let target = PathBuf::from(index.to_string());
        inventory.insert(target.clone(), CachedEntry::Directory);
        capture_snapshot_file(
            package,
            &package.join("plugin.json"),
            &target.join("plugin.json"),
            &mut inventory,
            &mut captured_bytes,
        )?;
    }
    let expected_skills = skill_directories
        .iter()
        .map(|directory| {
            let (index, package) = owning_package(package_roots, directory).ok_or_else(|| {
                format!(
                    "plugin skill {} is outside its package",
                    directory.display()
                )
            })?;
            let relative = directory
                .strip_prefix(package)
                .expect("owning package is a prefix");
            let target = PathBuf::from(index.to_string()).join(relative);
            collect_skill_inventory(
                package,
                directory,
                &PathBuf::from(index.to_string()),
                &mut inventory,
                &mut captured_bytes,
            )?;
            Ok(target)
        })
        .collect::<Result<BTreeSet<_>, String>>()?;

    // A second complete capture rejects in-place edits and inventory changes
    // that race the first directory traversal.
    let mut verification = BTreeMap::<PathBuf, CachedEntry>::new();
    let mut verification_bytes = 0_u64;
    for (index, package) in package_roots.iter().enumerate() {
        let target = PathBuf::from(index.to_string());
        verification.insert(target.clone(), CachedEntry::Directory);
        capture_snapshot_file(
            package,
            &package.join("plugin.json"),
            &target.join("plugin.json"),
            &mut verification,
            &mut verification_bytes,
        )?;
    }
    for directory in skill_directories {
        let (index, package) = owning_package(package_roots, directory).ok_or_else(|| {
            format!(
                "plugin skill {} is outside its package",
                directory.display()
            )
        })?;
        collect_skill_inventory(
            package,
            directory,
            &PathBuf::from(index.to_string()),
            &mut verification,
            &mut verification_bytes,
        )?;
    }
    if verification != inventory {
        return Err("plugin skills changed while their generation was being captured".into());
    }

    let paths = inventory.keys().cloned().collect::<Vec<_>>();
    for path in paths {
        let mut parent = path.parent();
        while let Some(relative) = parent.filter(|parent| !parent.as_os_str().is_empty()) {
            match inventory.get(relative) {
                Some(CachedEntry::File(_)) => {
                    return Err(format!(
                        "plugin skill inventory uses a file as a directory: {}",
                        relative.display()
                    ));
                }
                Some(CachedEntry::Directory) => {}
                None => {
                    inventory.insert(relative.to_path_buf(), CachedEntry::Directory);
                }
            }
            parent = relative.parent();
        }
    }

    let mut fingerprint = blake3::Hasher::new();
    for root in package_roots {
        fingerprint.update(root.as_os_str().as_encoded_bytes());
        fingerprint.update(&[0]);
    }
    for (relative, entry) in &inventory {
        fingerprint.update(&(relative.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
        fingerprint.update(relative.as_os_str().as_encoded_bytes());
        match entry {
            CachedEntry::Directory => {
                fingerprint.update(&[0]);
            }
            CachedEntry::File(bytes) => {
                fingerprint.update(&[1]);
                fingerprint.update(&(bytes.len() as u64).to_le_bytes());
                fingerprint.update(bytes);
            }
        }
    }
    let parent = cache_root.join("skill-generations");
    let destination = parent.join(fingerprint.finalize().to_hex().as_str());
    publish_cached_directory(&destination, "plugin skill generation", |staging| {
        write_cached_inventory(staging, &inventory)?;
        validate_skill_snapshot(staging, package_roots.len(), &expected_skills).map(|_| ())
    })?;
    validate_cached_inventory(&destination, &inventory)?;
    let published = validate_skill_snapshot(&destination, package_roots.len(), &expected_skills)?;
    make_tree_read_only(&destination)?;
    Ok(published)
}

fn cleanup_runtime_skill_generations(root: &Path, current: &ResolvedPlugins) {
    let generations = root.join("skill-generations");
    let current = current
        .package_roots
        .iter()
        .filter_map(|package| package.parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>();
    let Ok(entries) = fs::read_dir(&generations) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !current.contains(&path) {
            make_tree_writable(&path);
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn make_tree_read_only(root: &Path) -> Result<(), String> {
    let mut paths = vec![root.to_path_buf()];
    let mut index = 0;
    while index < paths.len() {
        let path = paths[index].clone();
        index += 1;
        if path.is_dir() {
            for entry in fs::read_dir(&path).map_err(|error| {
                format!(
                    "could not inspect plugin snapshot {}: {error}",
                    path.display()
                )
            })? {
                paths.push(
                    entry
                        .map_err(|error| format!("could not inspect plugin snapshot: {error}"))?
                        .path(),
                );
            }
        }
    }
    for path in paths.into_iter().rev() {
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "could not inspect plugin snapshot {}: {error}",
                path.display()
            )
        })?;
        let permissions = read_only_permissions(&metadata);
        fs::set_permissions(&path, permissions).map_err(|error| {
            format!(
                "could not make plugin snapshot read-only {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn make_tree_writable(root: &Path) {
    let mut paths = vec![root.to_path_buf()];
    let mut index = 0;
    while index < paths.len() {
        let path = paths[index].clone();
        index += 1;
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.permissions().readonly() {
            let _ = fs::set_permissions(&path, writable_permissions(&metadata));
        }
        if metadata.is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            paths.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
}

#[cfg(test)]
pub(crate) fn make_tree_writable_for_test(root: &Path) {
    make_tree_writable(root);
}

#[cfg(unix)]
fn read_only_permissions(metadata: &fs::Metadata) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(metadata.permissions().mode() & !0o222)
}

#[cfg(not(unix))]
fn read_only_permissions(metadata: &fs::Metadata) -> fs::Permissions {
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    permissions
}

#[cfg(unix)]
fn writable_permissions(metadata: &fs::Metadata) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(metadata.permissions().mode() | 0o200)
}

#[cfg(not(unix))]
fn writable_permissions(metadata: &fs::Metadata) -> fs::Permissions {
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    permissions
}

fn owning_package<'a>(package_roots: &'a [PathBuf], path: &Path) -> Option<(usize, &'a Path)> {
    package_roots
        .iter()
        .enumerate()
        .filter(|(_, package)| path.starts_with(package))
        .max_by_key(|(_, package)| package.as_os_str().as_encoded_bytes().len())
        .map(|(index, package)| (index, package.as_path()))
}

fn collect_skill_inventory(
    package_root: &Path,
    directory: &Path,
    target_root: &Path,
    inventory: &mut BTreeMap<PathBuf, CachedEntry>,
    captured_bytes: &mut u64,
) -> Result<(), String> {
    let canonical = directory.canonicalize().map_err(|error| {
        format!(
            "could not resolve plugin skill {}: {error}",
            directory.display()
        )
    })?;
    if !canonical.starts_with(package_root) {
        return Err(format!(
            "plugin skill path resolves outside its package: {}",
            directory.display()
        ));
    }
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        format!(
            "could not inspect plugin skill {}: {error}",
            directory.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "plugin skill path is not a real directory: {}",
            directory.display()
        ));
    }
    let relative = directory
        .strip_prefix(package_root)
        .expect("validated skill directory is in its package");
    inventory.insert(target_root.join(relative), CachedEntry::Directory);
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not read plugin skill {}: {error}",
            directory.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read plugin skill entry: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!("could not inspect plugin skill {}: {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "plugin skill path is a symlink: {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_skill_inventory(package_root, &path, target_root, inventory, captured_bytes)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(package_root)
                .expect("skill inventory remains under its package");
            capture_snapshot_file(
                package_root,
                &path,
                &target_root.join(relative),
                inventory,
                captured_bytes,
            )?;
        } else {
            return Err(format!(
                "plugin skill path is not a regular file: {}",
                path.display()
            ));
        }
    }
    let after = fs::symlink_metadata(directory).map_err(|error| {
        format!(
            "could not recheck plugin skill {}: {error}",
            directory.display()
        )
    })?;
    let canonical_after = directory.canonicalize().map_err(|error| {
        format!(
            "could not re-resolve plugin skill {}: {error}",
            directory.display()
        )
    })?;
    if !after.is_dir()
        || after.file_type().is_symlink()
        || !same_file_state(&metadata, &after)
        || canonical_after != canonical
    {
        return Err(format!(
            "plugin skill directory changed while it was being captured: {}",
            directory.display()
        ));
    }
    Ok(())
}

fn capture_snapshot_file(
    package_root: &Path,
    source: &Path,
    target: &Path,
    inventory: &mut BTreeMap<PathBuf, CachedEntry>,
    captured_bytes: &mut u64,
) -> Result<(), String> {
    let canonical = source.canonicalize().map_err(|error| {
        format!(
            "could not resolve plugin skill {}: {error}",
            source.display()
        )
    })?;
    if !canonical.starts_with(package_root) {
        return Err(format!(
            "plugin skill path resolves outside its package: {}",
            source.display()
        ));
    }
    let before = fs::symlink_metadata(source).map_err(|error| {
        format!(
            "could not inspect plugin skill {}: {error}",
            source.display()
        )
    })?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(format!(
            "plugin skill path is not a real file: {}",
            source.display()
        ));
    }
    let mut file = open_snapshot_file(package_root, source)?;
    let opened = file.metadata().map_err(|error| {
        format!(
            "could not inspect open plugin skill {}: {error}",
            source.display()
        )
    })?;
    let after = fs::symlink_metadata(source).map_err(|error| {
        format!(
            "could not recheck plugin skill {}: {error}",
            source.display()
        )
    })?;
    if after.file_type().is_symlink()
        || !opened.is_file()
        || !after.is_file()
        || !same_file(&before, &opened)
        || !same_file(&opened, &after)
    {
        return Err(format!(
            "plugin skill changed while it was being captured: {}",
            source.display()
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read plugin skill {}: {error}", source.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "plugin skill file exceeds size limit: {}",
            source.display()
        ));
    }
    file.rewind().map_err(|error| {
        format!(
            "could not recheck plugin skill {}: {error}",
            source.display()
        )
    })?;
    let mut second = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut second)
        .map_err(|error| {
            format!(
                "could not recheck plugin skill {}: {error}",
                source.display()
            )
        })?;
    let opened_after = file.metadata().map_err(|error| {
        format!(
            "could not recheck open plugin skill {}: {error}",
            source.display()
        )
    })?;
    let path_after = fs::symlink_metadata(source).map_err(|error| {
        format!(
            "could not recheck plugin skill {}: {error}",
            source.display()
        )
    })?;
    if bytes != second
        || !same_file_state(&before, &opened_after)
        || !same_file_state(&opened_after, &path_after)
    {
        return Err(format!(
            "plugin skill changed while it was being captured: {}",
            source.display()
        ));
    }
    *captured_bytes = captured_bytes
        .checked_add(bytes.len() as u64)
        .filter(|bytes| *bytes <= MAX_EXPANDED_BYTES)
        .ok_or_else(|| "plugin skills exceed expanded size limit".to_string())?;
    inventory.insert(target.to_path_buf(), CachedEntry::File(bytes));
    Ok(())
}

#[cfg(unix)]
fn open_snapshot_file(package_root: &Path, source: &Path) -> Result<fs::File, String> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
    };

    let relative = source
        .strip_prefix(package_root)
        .map_err(|_| format!("plugin skill is outside its package: {}", source.display()))?;
    let root = CString::new(package_root.as_os_str().as_bytes())
        .map_err(|_| "plugin package path contains a NUL byte".to_string())?;
    // SAFETY: `root` is NUL terminated and the returned descriptor is owned.
    let descriptor = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(format!(
            "could not open plugin package {} without following links: {}",
            package_root.display(),
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `descriptor` was returned uniquely by `open` above.
    let mut current = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err(format!("invalid plugin skill path: {}", source.display()));
        };
        let component = CString::new(component.as_bytes())
            .map_err(|_| "plugin skill path contains a NUL byte".to_string())?;
        let last = index + 1 == components.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: both the directory descriptor and component C string are valid.
        let descriptor = unsafe { libc::openat(current.as_raw_fd(), component.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(format!(
                "could not open plugin skill {} without following links: {}",
                source.display(),
                io::Error::last_os_error()
            ));
        }
        // SAFETY: `descriptor` was returned uniquely by `openat` above.
        current = unsafe { OwnedFd::from_raw_fd(descriptor) };
    }
    Ok(fs::File::from(current))
}

#[cfg(not(unix))]
fn open_snapshot_file(_package_root: &Path, source: &Path) -> Result<fs::File, String> {
    OpenOptions::new()
        .read(true)
        .open(source)
        .map_err(|error| format!("could not open plugin skill {}: {error}", source.display()))
}

fn same_file_state(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file(left, right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn write_cached_inventory(
    root: &Path,
    inventory: &BTreeMap<PathBuf, CachedEntry>,
) -> Result<(), String> {
    for (relative, entry) in inventory {
        let path = root.join(relative);
        match entry {
            CachedEntry::Directory => fs::create_dir_all(&path).map_err(|error| {
                format!(
                    "could not create cached plugin directory {}: {error}",
                    path.display()
                )
            })?,
            CachedEntry::File(bytes) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!(
                            "could not create cached plugin directory {}: {error}",
                            parent.display()
                        )
                    })?;
                }
                fs::write(&path, bytes).map_err(|error| {
                    format!(
                        "could not write cached plugin file {}: {error}",
                        path.display()
                    )
                })?;
            }
        }
    }
    Ok(())
}

fn validate_cached_inventory(
    root: &Path,
    expected: &BTreeMap<PathBuf, CachedEntry>,
) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    let mut actual = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| {
            format!(
                "could not read immutable plugin skill snapshot {}: {error}",
                directory.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                format!("could not read immutable plugin skill snapshot entry: {error}")
            })?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|_| {
                format!(
                    "immutable plugin skill snapshot entry escaped its root: {}",
                    path.display()
                )
            })?;
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!(
                    "could not inspect immutable plugin skill snapshot {}: {error}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "immutable plugin skill snapshot contains a symlink: {}",
                    path.display()
                ));
            }
            if actual.len() >= MAX_ARCHIVE_ENTRIES {
                return Err("immutable plugin skill snapshot exceeds entry limit".into());
            }
            actual.insert(relative.to_path_buf());
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(format!(
                    "immutable plugin skill snapshot entry is not a regular file: {}",
                    path.display()
                ));
            }
        }
    }
    if actual != expected.keys().cloned().collect() {
        return Err(
            "immutable plugin skill snapshot inventory does not match its fingerprint".into(),
        );
    }
    for (relative, expected_entry) in expected {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "could not inspect immutable plugin skill snapshot {}: {error}",
                path.display()
            )
        })?;
        match expected_entry {
            CachedEntry::Directory if metadata.is_dir() => {}
            CachedEntry::File(expected_bytes) if metadata.is_file() => {
                if metadata.len() > MAX_FILE_BYTES {
                    return Err(format!(
                        "immutable plugin skill snapshot file exceeds size limit: {}",
                        path.display()
                    ));
                }
                let bytes = fs::read(&path).map_err(|error| {
                    format!(
                        "could not read immutable plugin skill snapshot {}: {error}",
                        path.display()
                    )
                })?;
                if &bytes != expected_bytes {
                    return Err(format!(
                        "immutable plugin skill snapshot does not match its fingerprint: {}",
                        path.display()
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "immutable plugin skill snapshot has the wrong entry type: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn validate_skill_snapshot(
    root: &Path,
    package_count: usize,
    expected_skills: &BTreeSet<PathBuf>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "could not resolve immutable plugin skill snapshot {}: {error}",
            root.display()
        )
    })?;
    let mut package_roots = Vec::new();
    let mut skill_directories = Vec::new();
    let mut actual_skills = BTreeSet::new();
    for index in 0..package_count {
        let package = root.join(index.to_string());
        let plugin = AgentPlugin::load(&package).map_err(|error| {
            format!(
                "could not validate immutable plugin skill snapshot {}: {error}",
                package.display()
            )
        })?;
        validate_plugin_diagnostics(&index.to_string(), &plugin)?;
        if !plugin.skill_directories().is_empty() {
            package_roots.push(package.clone());
        }
        for skill in plugin.skill_directories() {
            let relative = skill.strip_prefix(&canonical_root).map_err(|_| {
                format!(
                    "immutable plugin skill escaped its snapshot root: {}",
                    skill.display()
                )
            })?;
            actual_skills.insert(relative.to_path_buf());
            skill_directories.push(root.join(relative));
        }
    }
    if &actual_skills != expected_skills {
        return Err("plugin skills changed while the immutable generation was being built".into());
    }
    Ok((package_roots, skill_directories))
}

fn publish_cached_directory(
    destination: &Path,
    context: &str,
    build: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(format!(
                "{context} cache entry is not a real directory: {}",
                destination.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not inspect {context} cache entry: {error}")),
    }
    let parent = destination
        .parent()
        .ok_or_else(|| format!("{context} cache path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create {context} cache {}: {error}",
            parent.display()
        )
    })?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let staging = parent.join(format!(
        ".{}-{:x}.tmp",
        std::process::id(),
        u64::from_le_bytes(random)
    ));
    fs::create_dir(&staging)
        .map_err(|error| format!("could not create {context} staging directory: {error}"))?;
    let result = build(&staging).and_then(|()| match fs::rename(&staging, destination) {
        Ok(()) => Ok(()),
        Err(_)
            if fs::symlink_metadata(destination)
                .is_ok_and(|metadata| metadata.file_type().is_dir()) =>
        {
            Ok(())
        }
        Err(error) => Err(format!("could not publish {context}: {error}")),
    });
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn validate_plugin_diagnostics(alias: &str, plugin: &AgentPlugin) -> Result<(), String> {
    for diagnostic in plugin.diagnostics() {
        if matches!(diagnostic.kind, PluginDiagnosticKind::UnknownManifestField) {
            continue;
        }
        let path = diagnostic
            .path
            .as_deref()
            .map(|path| format!(" at {}", path.display()))
            .unwrap_or_default();
        return Err(format!(
            "plugin {alias} ({:?}){path}: {}",
            diagnostic.kind, diagnostic.message
        ));
    }
    Ok(())
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum GitRevision {
    Commit(String),
    Ref(String),
    DefaultBranch,
}

#[derive(Clone, Copy)]
enum GitProtocol {
    Https,
    #[cfg(test)]
    Local,
}

#[derive(Debug, PartialEq, Eq)]
enum GitFailure {
    Unavailable(io::ErrorKind),
    Timeout,
    OutputLimit,
    ObjectStoreLimit,
    ObjectStoreInspection(io::ErrorKind),
    Archive(String),
    Exit(Option<i32>),
}

struct GitRunRequest<'a> {
    cwd: &'a Path,
    args: &'a [OsString],
    config: &'a [(OsString, OsString)],
    stdout_limit: u64,
    stderr_limit: u64,
    object_store_limit: Option<&'a Path>,
}

trait GitRunner: Sync {
    fn run(&self, request: GitRunRequest<'_>) -> Result<Vec<u8>, GitFailure>;

    fn archive(&self, request: GitRunRequest<'_>, destination: &Path) -> Result<(), GitFailure>;
}

struct SystemGitRunner {
    timeout: Duration,
}

impl Default for SystemGitRunner {
    fn default() -> Self {
        Self {
            timeout: GIT_COMMAND_TIMEOUT,
        }
    }
}

enum GitStdout {
    Capture(u64),
    Archive { destination: PathBuf, limit: u64 },
}

enum GitPipeEvent {
    Stdout(Result<Vec<u8>, GitFailure>),
    Stderr(Result<(), GitFailure>),
}

impl SystemGitRunner {
    fn execute(
        &self,
        request: GitRunRequest<'_>,
        stdout_mode: GitStdout,
    ) -> Result<Vec<u8>, GitFailure> {
        let mut command = Command::new("git");
        command
            .args(request.args)
            .current_dir(request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, _) in env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        command.env("GIT_CONFIG_COUNT", request.config.len().to_string());
        for (index, (key, value)) in request.config.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        command
            .env_remove("SSH_ASKPASS")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("LC_ALL", "C");
        isolate_process_tree(&mut command);

        let mut child = command
            .spawn()
            .map_err(|error| GitFailure::Unavailable(error.kind()))?;
        let child_pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| {
            terminate_process_tree_with_pid(&mut child, child_pid);
            GitFailure::Unavailable(io::ErrorKind::BrokenPipe)
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            terminate_process_tree_with_pid(&mut child, child_pid);
            GitFailure::Unavailable(io::ErrorKind::BrokenPipe)
        })?;
        let (sender, receiver) = mpsc::channel();
        let stdout_sender = sender.clone();
        let stdout_thread = thread::spawn(move || {
            let result = match stdout_mode {
                GitStdout::Capture(limit) => read_bounded(stdout, limit),
                GitStdout::Archive { destination, limit } => {
                    let result = extract_tar(
                        LimitedReader {
                            inner: stdout,
                            remaining: limit,
                        },
                        &destination,
                    );
                    match result {
                        Ok(()) => Ok(Vec::new()),
                        Err(error)
                            if error.contains("plugin tar stream exceeds expanded size limit") =>
                        {
                            Err(GitFailure::OutputLimit)
                        }
                        Err(error) => Err(GitFailure::Archive(error)),
                    }
                }
            };
            let _ = stdout_sender.send(GitPipeEvent::Stdout(result));
        });
        let stderr_limit = request.stderr_limit;
        let stderr_thread = thread::spawn(move || {
            let result = drain_bounded(stderr, stderr_limit);
            let _ = sender.send(GitPipeEvent::Stderr(result));
        });

        let started = Instant::now();
        let mut next_object_scan = started + GIT_OBJECT_SCAN_INITIAL_DELAY;
        let mut object_scan_delay = GIT_OBJECT_SCAN_INITIAL_DELAY;
        let mut process_done = false;
        let mut status = None;
        let mut stdout_result = None;
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut failure = None;
        let mut tree_terminated = false;

        while !process_done || !stdout_done || !stderr_done {
            if !stdout_done || !stderr_done {
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(GitPipeEvent::Stdout(result)) => {
                        stdout_done = true;
                        match result {
                            Ok(bytes) => stdout_result = Some(bytes),
                            Err(error) => {
                                failure.get_or_insert(error);
                                terminate_process_tree_with_pid(&mut child, child_pid);
                                tree_terminated = true;
                                process_done = true;
                            }
                        }
                    }
                    Ok(GitPipeEvent::Stderr(result)) => {
                        stderr_done = true;
                        if let Err(error) = result {
                            failure.get_or_insert(error);
                            terminate_process_tree_with_pid(&mut child, child_pid);
                            tree_terminated = true;
                            process_done = true;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        failure.get_or_insert(GitFailure::Unavailable(io::ErrorKind::BrokenPipe));
                        break;
                    }
                }
            }

            let now = Instant::now();
            if now.duration_since(started) >= self.timeout {
                failure.get_or_insert(GitFailure::Timeout);
                terminate_process_tree_with_pid(&mut child, child_pid);
                tree_terminated = true;
                break;
            }

            if failure.is_some() {
                if !tree_terminated {
                    terminate_process_tree_with_pid(&mut child, child_pid);
                    tree_terminated = true;
                }
                process_done = true;
                continue;
            }
            if process_done {
                continue;
            }

            if let Some(path) = request.object_store_limit
                && now >= next_object_scan
            {
                match inspect_git_object_store(path) {
                    Ok(()) => {}
                    Err(error) => {
                        failure = Some(error);
                        terminate_process_tree_with_pid(&mut child, child_pid);
                        tree_terminated = true;
                        process_done = true;
                        continue;
                    }
                }
                object_scan_delay = (object_scan_delay * 2).min(GIT_OBJECT_SCAN_MAX_DELAY);
                next_object_scan = now + object_scan_delay;
            }

            match child.try_wait() {
                Ok(Some(value)) => {
                    status = Some(value);
                    // A successful Git parent may leave helpers holding the pipes.
                    terminate_process_tree_with_pid(&mut child, child_pid);
                    tree_terminated = true;
                    process_done = true;
                }
                Ok(None) => {}
                Err(error) => {
                    failure = Some(GitFailure::Unavailable(error.kind()));
                    terminate_process_tree_with_pid(&mut child, child_pid);
                    tree_terminated = true;
                    process_done = true;
                }
            }
        }

        if failure.is_some() && !tree_terminated {
            terminate_process_tree_with_pid(&mut child, child_pid);
        }
        let stdout_joined = stdout_thread.join().is_ok();
        let stderr_joined = stderr_thread.join().is_ok();
        if !stdout_joined || !stderr_joined {
            failure.get_or_insert(GitFailure::Timeout);
        }
        if let Some(path) = request.object_store_limit
            && failure.is_none()
            && let Err(error) = inspect_git_object_store(path)
        {
            failure = Some(error);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let status = status.ok_or(GitFailure::Unavailable(io::ErrorKind::BrokenPipe))?;
        if !status.success() {
            return Err(GitFailure::Exit(status.code()));
        }
        stdout_result.ok_or(GitFailure::Unavailable(io::ErrorKind::BrokenPipe))
    }
}

impl GitRunner for SystemGitRunner {
    fn run(&self, request: GitRunRequest<'_>) -> Result<Vec<u8>, GitFailure> {
        let limit = request.stdout_limit;
        self.execute(request, GitStdout::Capture(limit))
    }

    fn archive(&self, request: GitRunRequest<'_>, destination: &Path) -> Result<(), GitFailure> {
        let limit = request.stdout_limit;
        self.execute(
            request,
            GitStdout::Archive {
                destination: destination.to_path_buf(),
                limit,
            },
        )
        .map(|_| ())
    }
}

fn read_bounded(mut reader: impl Read, limit: u64) -> Result<Vec<u8>, GitFailure> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| GitFailure::Unavailable(error.kind()))?;
    if bytes.len() as u64 > limit {
        return Err(GitFailure::OutputLimit);
    }
    Ok(bytes)
}

fn drain_bounded(mut reader: impl Read, limit: u64) -> Result<(), GitFailure> {
    let mut buffer = [0u8; 8192];
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| GitFailure::Unavailable(error.kind()))?;
        if read == 0 {
            return Ok(());
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(GitFailure::OutputLimit);
        }
    }
}

fn inspect_git_object_store(path: &Path) -> Result<(), GitFailure> {
    let bytes = git_object_store_size(path)
        .map_err(|error| GitFailure::ObjectStoreInspection(error.kind()))?;
    if bytes > MAX_EXPANDED_BYTES {
        return Err(GitFailure::ObjectStoreLimit);
    }
    Ok(())
}

fn enforce_git_staging_metadata(git_dir: &Path, remote: &OsStr) -> Result<(), String> {
    let remote = remote
        .to_str()
        .ok_or("plugin Git URL must be valid Unicode")?
        .as_bytes();
    if remote.is_empty() {
        return Err("plugin Git remote must not be empty".into());
    }

    let mut pending = vec![git_dir.to_path_buf()];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = pending.pop() {
        let contents = fs::read_dir(&directory)
            .map_err(|_| "could not inspect Git plugin staging metadata")?;
        for entry in contents {
            let entry = entry.map_err(|_| "could not inspect Git plugin staging metadata")?;
            let name = entry.file_name();
            if name == OsStr::new("FETCH_HEAD") {
                return Err("Git plugin fetch wrote forbidden FETCH_HEAD metadata".into());
            }
            if directory == git_dir && name == OsStr::new("objects") {
                continue;
            }
            entries += 1;
            if entries > MAX_GIT_OBJECT_ENTRIES {
                return Err("Git plugin staging metadata contains too many entries".into());
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "could not inspect Git plugin staging metadata")?;
            if metadata.file_type().is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.file_type().is_file() {
                return Err("Git plugin staging metadata contains an unsupported entry".into());
            }
            let remaining = MAX_GIT_METADATA_BYTES.saturating_sub(bytes);
            let mut contents = Vec::new();
            File::open(path)
                .and_then(|file| file.take(remaining + 1).read_to_end(&mut contents))
                .map_err(|_| "could not read Git plugin staging metadata")?;
            bytes = bytes
                .checked_add(contents.len() as u64)
                .ok_or("Git plugin staging metadata size overflow")?;
            if bytes > MAX_GIT_METADATA_BYTES {
                return Err("Git plugin staging metadata exceeds its size limit".into());
            }
            if contents
                .windows(remote.len())
                .any(|candidate| candidate == remote)
            {
                return Err("Git plugin staging metadata contains the configured remote".into());
            }
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

fn random_suffix() -> Result<String, String> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct StagingDirectory {
    path: PathBuf,
}

impl StagingDirectory {
    fn create(parent: &Path, prefix: &str) -> Result<Self, String> {
        cleanup_stale_staging(parent, prefix, STALE_STAGING_AGE)?;
        let path = parent.join(format!("{prefix}{}", random_suffix()?));
        create_private_directory(&path)
            .map_err(|error| format!("could not create plugin staging directory: {error}"))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn cleanup_stale_staging(parent: &Path, prefix: &str, age: Duration) -> Result<(), String> {
    let entries = fs::read_dir(parent)
        .map_err(|error| format!("could not inspect plugin staging directories: {error}"))?;
    let now = SystemTime::now();
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("could not inspect plugin staging directory: {error}"))?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(prefix)) else {
            continue;
        };
        if suffix.len() != 16
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "could not inspect stale plugin staging directory: {error}"
                ));
            }
        };
        if !metadata.file_type().is_dir()
            || metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|elapsed| elapsed < age)
        {
            continue;
        }
        fs::remove_dir_all(path)
            .map_err(|error| format!("could not remove stale plugin staging directory: {error}"))?;
    }
    Ok(())
}

fn resolve_git(
    value: &str,
    rev: Option<&str>,
    subdir: Option<&str>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let url = validate_git_url(value)?;
    let revision = rev
        .map(validate_git_revision)
        .transpose()?
        .unwrap_or(GitRevision::DefaultBranch);
    let subdir = subdir.map(validate_git_subdir).transpose()?;
    let source = url.as_str();
    let source_key = sha256_text(source);
    resolve_git_source(
        OsStr::new(source),
        &source_key,
        &revision,
        subdir.as_deref(),
        cache_root,
        GitProtocol::Https,
        &SystemGitRunner::default(),
    )
}

#[cfg(test)]
fn resolve_git_local(
    repository: &Path,
    rev: &str,
    subdir: Option<&Path>,
    cache_root: &Path,
    runner: &dyn GitRunner,
) -> Result<PathBuf, String> {
    resolve_git_local_revision(
        repository,
        validate_git_revision(rev)?,
        subdir,
        cache_root,
        runner,
    )
}

#[cfg(test)]
fn resolve_git_local_revision(
    repository: &Path,
    revision: GitRevision,
    subdir: Option<&Path>,
    cache_root: &Path,
    runner: &dyn GitRunner,
) -> Result<PathBuf, String> {
    let repository = repository
        .canonicalize()
        .map_err(|error| format!("could not resolve test Git repository: {error}"))?;
    let subdir = subdir
        .map(|path| {
            path.to_str()
                .ok_or_else(|| "plugin Git subdir must be valid Unicode".to_string())
                .and_then(validate_git_subdir)
        })
        .transpose()?;
    let source_key = sha256_text(&repository.to_string_lossy());
    resolve_git_source(
        repository.as_os_str(),
        &source_key,
        &revision,
        subdir.as_deref(),
        cache_root,
        GitProtocol::Local,
        runner,
    )
}

fn validate_git_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "invalid plugin Git URL".to_string())?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "plugin Git URL must be an absolute HTTPS URL without credentials, query, or fragment"
                .into(),
        );
    }
    Ok(url)
}

fn validate_git_revision(value: &str) -> Result<GitRevision, String> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(GitRevision::Commit(value.to_ascii_lowercase()));
    }
    if value == "HEAD" {
        return Ok(GitRevision::DefaultBranch);
    }
    if !valid_git_ref_name(value) {
        return Err("invalid plugin Git revision name".into());
    }
    Ok(GitRevision::Ref(value.to_owned()))
}

fn valid_git_ref_name(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 1024
        || value == "@"
        || value.starts_with(['/', '-', '+'])
        || value.ends_with('/')
        || value.ends_with('.')
        || value.contains("//")
        || value.contains("..")
        || value.contains("@{")
        || value.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return false;
    }
    value.split('/').all(|component| {
        !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
    })
}

fn validate_git_subdir(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.ends_with('/')
        || raw.contains('\\')
        || raw
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err("plugin Git subdir must be a canonical contained relative path".into());
    }

    let mut normalized = PathBuf::new();
    for value in raw.split('/') {
        let upper = value
            .split('.')
            .next()
            .unwrap_or(value)
            .to_ascii_uppercase();
        let reserved = matches!(
            upper.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "COM¹" | "COM²" | "COM³" | "LPT¹" | "LPT²" | "LPT³"
        ) || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
        if value.is_empty()
            || value.ends_with(['.', ' '])
            || value.chars().any(|character| {
                character.is_control()
                    || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\')
            })
            || reserved
        {
            return Err("plugin Git subdir is not portable".into());
        }
        normalized.push(value);
    }
    Ok(normalized)
}

fn sha256_text(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hardened_git_args(
    protocol: GitProtocol,
    hooks: &Path,
    attributes: &Path,
    args: &[&OsStr],
) -> Vec<OsString> {
    let mut output = vec![
        OsString::from("-c"),
        OsString::from("protocol.allow=never"),
        OsString::from("-c"),
        OsString::from("protocol.https.allow=always"),
        OsString::from("-c"),
        OsString::from("protocol.file.allow=never"),
        OsString::from("-c"),
        OsString::from("protocol.ext.allow=never"),
        OsString::from("-c"),
        OsString::from("http.followRedirects=false"),
        OsString::from("-c"),
        OsString::from("http.sslVerify=true"),
        OsString::from("-c"),
        OsString::from("fetch.recurseSubmodules=false"),
        OsString::from("-c"),
        OsString::from("submodule.recurse=false"),
        OsString::from("-c"),
        OsString::from("maintenance.auto=false"),
        OsString::from("-c"),
        OsString::from("gc.auto=0"),
        OsString::from("-c"),
        OsString::from("core.logAllRefUpdates=false"),
        OsString::from("-c"),
        OsString::from("fetch.writeCommitGraph=false"),
        OsString::from("-c"),
        OsString::from("fetch.fsckObjects=true"),
        OsString::from("-c"),
        OsString::from("transfer.fsckObjects=true"),
        OsString::from("-c"),
        OsString::from(format!("core.hooksPath={}", hooks.display())),
        OsString::from("-c"),
        OsString::from(format!("init.templateDir={}", hooks.display())),
        OsString::from("-c"),
        OsString::from(format!("core.attributesFile={}", attributes.display())),
    ];
    #[cfg(test)]
    if matches!(protocol, GitProtocol::Local) {
        output.extend([
            OsString::from("-c"),
            OsString::from("protocol.file.allow=always"),
        ]);
    }
    #[cfg(not(test))]
    let _ = protocol;
    output.extend(args.iter().map(|value| (*value).to_os_string()));
    output
}

fn hardened_git_config(remote: Option<&str>) -> Vec<(OsString, OsString)> {
    let mut config = vec![
        (OsString::from("core.askPass"), OsString::new()),
        (
            OsString::from("credential.interactive"),
            OsString::from("false"),
        ),
    ];
    if let Some(remote) = remote {
        config.extend([
            (
                OsString::from(format!("http.{remote}.followRedirects")),
                OsString::from("false"),
            ),
            (
                OsString::from(format!("http.{remote}.sslVerify")),
                OsString::from("true"),
            ),
        ]);
    }
    config
}

struct GitCommandContext<'a> {
    runner: &'a dyn GitRunner,
    protocol: GitProtocol,
    hooks: &'a Path,
    attributes: &'a Path,
    remote: Option<&'a str>,
}

impl GitCommandContext<'_> {
    fn run(
        &self,
        operation: &'static str,
        cwd: &Path,
        args: &[&OsStr],
        stdout_limit: u64,
        object_store_limit: Option<&Path>,
    ) -> Result<Vec<u8>, String> {
        let args = hardened_git_args(self.protocol, self.hooks, self.attributes, args);
        let config = hardened_git_config(self.remote);
        self.runner
            .run(GitRunRequest {
                cwd,
                args: &args,
                config: &config,
                stdout_limit,
                stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                object_store_limit,
            })
            .map_err(|failure| describe_git_failure(operation, failure))
    }

    fn archive(
        &self,
        operation: &'static str,
        cwd: &Path,
        args: &[&OsStr],
        destination: &Path,
    ) -> Result<(), String> {
        let args = hardened_git_args(self.protocol, self.hooks, self.attributes, args);
        let config = hardened_git_config(self.remote);
        self.runner
            .archive(
                GitRunRequest {
                    cwd,
                    args: &args,
                    config: &config,
                    stdout_limit: MAX_TAR_STREAM_BYTES,
                    stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                    object_store_limit: None,
                },
                destination,
            )
            .map_err(|failure| describe_git_failure(operation, failure))
    }
}

fn describe_git_failure(operation: &str, failure: GitFailure) -> String {
    match failure {
        GitFailure::Unavailable(kind) => {
            format!("could not run Git for plugin {operation}: {kind:?}")
        }
        GitFailure::Timeout => format!("Git plugin {operation} timed out"),
        GitFailure::OutputLimit => {
            format!("Git plugin {operation} exceeded its output limit")
        }
        GitFailure::ObjectStoreLimit => {
            "Git plugin fetch exceeded its object-store size limit".into()
        }
        GitFailure::ObjectStoreInspection(kind) => {
            format!("could not inspect Git plugin object store: {kind:?}")
        }
        GitFailure::Archive(error) => format!("invalid Git plugin archive: {error}"),
        GitFailure::Exit(code) => match code {
            Some(code) => format!("Git plugin {operation} failed with status {code}"),
            None => format!("Git plugin {operation} was terminated"),
        },
    }
}

fn resolve_git_source(
    remote: &OsStr,
    source_key: &str,
    revision: &GitRevision,
    subdir: Option<&Path>,
    cache_root: &Path,
    protocol: GitProtocol,
    runner: &dyn GitRunner,
) -> Result<PathBuf, String> {
    let subdir_text = subdir.map(path_to_git_string).transpose()?;
    let subdir_key = sha256_text(subdir_text.as_deref().unwrap_or("."));
    let source_root = cache_root.join(GIT_CACHE_VERSION).join(source_key);
    fs::create_dir_all(&source_root)
        .map_err(|error| format!("could not create Git plugin cache: {error}"))?;

    if let GitRevision::Commit(oid) = revision {
        let destination = git_cache_destination(&source_root, oid, &subdir_key);
        if let Ok(root) =
            validate_git_cache_entry(&destination, source_key, oid, &subdir_key, subdir)
        {
            return Ok(root);
        }
    }

    let remote_config = match protocol {
        GitProtocol::Https => Some(
            remote
                .to_str()
                .ok_or("plugin Git URL must be valid Unicode")?,
        ),
        #[cfg(test)]
        GitProtocol::Local => None,
    };
    let staging_guard = StagingDirectory::create(&source_root, ".staging-")?;
    let staging = staging_guard.path();
    let hooks = staging.join("hooks");
    let attributes = staging.join("attributes");
    let git_dir = staging.join("git");
    let repository = staging.join("repo");
    fs::create_dir(&hooks).map_err(|error| error.to_string())?;
    fs::create_dir(&git_dir).map_err(|error| error.to_string())?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&attributes)
        .map_err(|error| format!("could not create controlled Git attributes file: {error}"))?;
    let git = GitCommandContext {
        runner,
        protocol,
        hooks: &hooks,
        attributes: &attributes,
        remote: remote_config,
    };

    verify_git_effective_url(&git, staging, remote)?;

    git.run(
        "repository initialization",
        &git_dir,
        &[
            OsStr::new("init"),
            OsStr::new("--bare"),
            OsStr::new("--object-format=sha1"),
            OsStr::new("--"),
            OsStr::new("."),
        ],
        MAX_GIT_DIAGNOSTIC_BYTES,
        None,
    )?;
    fs::create_dir_all(git_dir.join("info"))
        .map_err(|error| format!("could not create Git info directory: {error}"))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(git_dir.join("info/attributes"))
        .map_err(|error| format!("could not create empty Git info attributes: {error}"))?;
    verify_git_effective_url(&git, &git_dir, remote)?;
    let fetch_revision = match revision {
        GitRevision::Commit(oid) | GitRevision::Ref(oid) => oid.as_str(),
        GitRevision::DefaultBranch => "HEAD",
    };
    let fetch_refspec = format!("{fetch_revision}:{GIT_PRIVATE_FETCH_REF}");
    git.run(
        "fetch",
        &git_dir,
        &[
            OsStr::new("fetch"),
            OsStr::new("--force"),
            OsStr::new("--depth=1"),
            OsStr::new("--no-tags"),
            OsStr::new("--no-recurse-submodules"),
            OsStr::new("--no-write-fetch-head"),
            OsStr::new("--"),
            remote,
            OsStr::new(&fetch_refspec),
        ],
        MAX_GIT_DIAGNOSTIC_BYTES,
        Some(&git_dir.join("objects")),
    )?;
    enforce_git_staging_metadata(&git_dir, remote)?;
    let private_commit = format!("{GIT_PRIVATE_FETCH_REF}^{{commit}}");
    let fetched = git.run(
        "commit verification",
        &git_dir,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--end-of-options"),
            OsStr::new(&private_commit),
        ],
        128,
        None,
    )?;
    let fetched = parse_git_oid(&fetched).ok_or("Git plugin revision is not a commit")?;
    if let GitRevision::Commit(expected) = revision
        && fetched != *expected
    {
        return Err("Git plugin fetch returned a different commit".into());
    }
    enforce_git_object_store_limit(&git_dir.join("objects"))?;

    let destination = git_cache_destination(&source_root, &fetched, &subdir_key);
    if let Ok(root) =
        validate_git_cache_entry(&destination, source_key, &fetched, &subdir_key, subdir)
    {
        return Ok(root);
    }

    let oid = OsStr::new(&fetched);
    let mut tree_args = vec![
        OsStr::new("ls-tree"),
        OsStr::new("-r"),
        OsStr::new("-l"),
        OsStr::new("-z"),
        oid,
    ];
    if let Some(path) = subdir_text.as_deref() {
        tree_args.extend([OsStr::new("--"), OsStr::new(path)]);
    }
    let tree = git.run(
        "tree inspection",
        &git_dir,
        &tree_args,
        MAX_GIT_TREE_BYTES,
        None,
    )?;
    validate_git_tree(&tree)?;

    let mut archive_args = vec![OsStr::new("archive"), OsStr::new("--format=tar"), oid];
    if let Some(path) = subdir_text.as_deref() {
        archive_args.extend([OsStr::new("--"), OsStr::new(path)]);
    }
    fs::create_dir(&repository).map_err(|error| error.to_string())?;
    git.archive("archive", &git_dir, &archive_args, &repository)?;
    let candidate = select_git_package_root(&repository, subdir)?;
    AgentPlugin::load(&candidate).map_err(|error| {
        format!(
            "invalid Git plugin package at {}: {error}",
            candidate.display()
        )
    })?;
    enforce_git_staging_metadata(&git_dir, remote)?;

    fs::remove_dir_all(&git_dir).map_err(|error| error.to_string())?;
    fs::remove_dir_all(&hooks).map_err(|error| error.to_string())?;
    fs::remove_file(&attributes).map_err(|error| error.to_string())?;
    write_git_cache_marker(staging, source_key, &fetched, &subdir_key)?;
    match fs::rename(staging, &destination) {
        Ok(()) => {}
        Err(_)
            if validate_git_cache_entry(
                &destination,
                source_key,
                &fetched,
                &subdir_key,
                subdir,
            )
            .is_ok() => {}
        Err(error) => return Err(format!("could not publish Git plugin cache: {error}")),
    }
    validate_git_cache_entry(&destination, source_key, &fetched, &subdir_key, subdir)
}

fn verify_git_effective_url(
    git: &GitCommandContext<'_>,
    cwd: &Path,
    remote: &OsStr,
) -> Result<(), String> {
    let expected = remote
        .to_str()
        .ok_or("plugin Git URL must be valid Unicode")?;
    let output = git.run(
        "URL verification",
        cwd,
        &[
            OsStr::new("ls-remote"),
            OsStr::new("--get-url"),
            OsStr::new("--"),
            remote,
        ],
        MAX_GIT_DIAGNOSTIC_BYTES,
        None,
    )?;
    let effective = output.strip_suffix(b"\n").unwrap_or(&output);
    let effective = effective.strip_suffix(b"\r").unwrap_or(effective);
    if effective != expected.as_bytes() {
        return Err("plugin Git URL is rewritten by Git configuration".into());
    }
    Ok(())
}

fn parse_git_oid(output: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(output).ok()?.trim();
    (value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn path_to_git_string(path: &Path) -> Result<String, String> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "plugin Git subdir must be valid Unicode".to_string()),
            _ => Err("plugin Git subdir must be a contained relative path".into()),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

fn validate_git_tree(output: &[u8]) -> Result<(), String> {
    let mut count = 0usize;
    let mut total = 0u64;
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err("Git plugin contains too many entries".into());
        }
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or("invalid Git tree response")?;
        let metadata =
            std::str::from_utf8(&record[..tab]).map_err(|_| "invalid Git tree response")?;
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err("invalid Git tree response".into());
        }
        if fields[0] == "120000" {
            return Err("Git plugin must not contain symbolic links".into());
        }
        if fields[0] == "160000" || fields[1] == "commit" {
            return Err("Git plugin must not contain submodules".into());
        }
        if fields[1] != "blob" {
            return Err("Git plugin contains an unsupported object".into());
        }
        let size = fields[3]
            .parse::<u64>()
            .map_err(|_| "invalid Git object size")?;
        total = total
            .checked_add(size)
            .ok_or("Git plugin object size overflow")?;
        if size > MAX_FILE_BYTES || total > MAX_EXPANDED_BYTES {
            return Err("Git plugin exceeds object size limits".into());
        }
    }
    Ok(())
}

fn git_object_store_size(objects: &Path) -> io::Result<u64> {
    git_object_store_size_with_limit(objects, MAX_GIT_OBJECT_ENTRIES)
}

fn git_object_store_size_with_limit(objects: &Path, entry_limit: usize) -> io::Result<u64> {
    if !fs::symlink_metadata(objects)?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git object store is not a directory",
        ));
    }
    let mut pending = vec![objects.to_path_buf()];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = pending.pop() {
        let contents = fs::read_dir(&directory)?;
        for entry in contents {
            let entry = entry?;
            entries += 1;
            if entries > entry_limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Git object store contains too many entries",
                ));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_dir() {
                pending.push(entry.path());
            } else if metadata.file_type().is_file() {
                bytes = bytes.checked_add(metadata.len()).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Git object store size overflow")
                })?;
                if bytes > MAX_EXPANDED_BYTES {
                    return Ok(bytes);
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Git object store contains an unsupported entry",
                ));
            }
        }
    }
    Ok(bytes)
}

fn enforce_git_object_store_limit(objects: &Path) -> Result<(), String> {
    let bytes =
        git_object_store_size(objects).map_err(|_| "could not inspect Git plugin object store")?;
    if bytes > MAX_EXPANDED_BYTES {
        return Err("Git plugin object store exceeds its size limit".into());
    }
    Ok(())
}

fn git_cache_destination(source_root: &Path, oid: &str, subdir_key: &str) -> PathBuf {
    source_root.join(format!("{oid}-{subdir_key}"))
}

fn git_cache_marker(source_key: &str, oid: &str, subdir_key: &str) -> String {
    format!("{GIT_CACHE_VERSION}\n{source_key}\n{oid}\n{subdir_key}\n")
}

fn write_git_cache_marker(
    directory: &Path,
    source_key: &str,
    oid: &str,
    subdir_key: &str,
) -> Result<(), String> {
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("complete"))
        .map_err(|error| format!("could not create Git plugin cache marker: {error}"))?;
    marker
        .write_all(git_cache_marker(source_key, oid, subdir_key).as_bytes())
        .and_then(|()| marker.sync_all())
        .map_err(|error| format!("could not write Git plugin cache marker: {error}"))
}

fn validate_git_cache_entry(
    directory: &Path,
    source_key: &str,
    oid: &str,
    subdir_key: &str,
    subdir: Option<&Path>,
) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| "Git plugin cache entry is missing".to_string())?;
    if !metadata.file_type().is_dir() {
        return Err("Git plugin cache entry is not a directory".into());
    }
    let entries = fs::read_dir(directory)
        .map_err(|_| "could not inspect Git plugin cache entry")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "could not inspect Git plugin cache entry")?;
    if entries.len() != 2
        || !entries.iter().any(|entry| entry.file_name() == "repo")
        || !entries.iter().any(|entry| entry.file_name() == "complete")
    {
        return Err("Git plugin cache entry contains unexpected files".into());
    }
    let repository_path = directory.join("repo");
    let repository = fs::symlink_metadata(&repository_path)
        .map_err(|_| "Git plugin cache repository is missing".to_string())?;
    if !repository.file_type().is_dir() {
        return Err("Git plugin cache repository is not a directory".into());
    }
    let marker_path = directory.join("complete");
    if !fs::symlink_metadata(&marker_path).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return Err("Git plugin cache entry is incomplete".into());
    }
    let expected = git_cache_marker(source_key, oid, subdir_key);
    let mut marker = Vec::new();
    File::open(marker_path)
        .and_then(|file| {
            file.take(expected.len() as u64 + 1)
                .read_to_end(&mut marker)
        })
        .map_err(|_| "could not read Git plugin cache marker")?;
    if marker != expected.as_bytes() {
        return Err("Git plugin cache entry is incomplete".into());
    }
    let root = select_git_package_root(&repository_path, subdir)?;
    AgentPlugin::load(&root)
        .map_err(|error| format!("invalid cached Git plugin package: {error}"))?;
    Ok(root)
}

fn select_git_package_root(repository: &Path, subdir: Option<&Path>) -> Result<PathBuf, String> {
    let canonical_repository = repository
        .canonicalize()
        .map_err(|error| format!("could not resolve Git plugin cache: {error}"))?;
    let selected = subdir.map_or_else(|| repository.to_path_buf(), |path| repository.join(path));
    let selected = selected
        .canonicalize()
        .map_err(|error| format!("could not resolve Git plugin subdir: {error}"))?;
    if !selected.is_dir() || !selected.starts_with(&canonical_repository) {
        return Err("plugin Git subdir escapes the repository".into());
    }
    Ok(selected)
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
    let bytes = cached_archive_bytes(
        &url,
        &expected,
        &cache_root.join("archive-blobs").join(&digest),
    )?;
    let destination = cache_root.join(&digest);
    if destination.exists() {
        verify_cached_archive(&destination, &bytes)?;
    }
    publish_cached_directory(&destination, "plugin archive", |staging| {
        extract_archive(&bytes, staging)?;
        let candidate = select_package_root(staging, subdir.as_deref())?;
        AgentPlugin::load(&candidate).map_err(|error| {
            format!(
                "invalid plugin archive package at {}: {error}",
                candidate.display()
            )
        })?;
        Ok(())
    })?;
    select_package_root(&destination, subdir.as_deref())
}

fn cached_archive_bytes(url: &Url, expected: &[u8; 32], blob: &Path) -> Result<Vec<u8>, String> {
    let bytes = match fs::symlink_metadata(blob) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= MAX_DOWNLOAD_BYTES => {
            fs::read(blob).map_err(|error| {
                format!(
                    "could not read cached plugin archive {}: {error}",
                    blob.display()
                )
            })?
        }
        Ok(_) => {
            return Err(format!(
                "cached plugin archive is not a bounded regular file: {}",
                blob.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let bytes = download(url)?;
            verify_archive_digest(url, expected, &bytes)?;
            publish_cached_file(blob, &bytes, "plugin archive blob")?;
            return cached_archive_bytes(url, expected, blob);
        }
        Err(error) => {
            return Err(format!(
                "could not inspect cached plugin archive {}: {error}",
                blob.display()
            ));
        }
    };
    verify_archive_digest(url, expected, &bytes)?;
    Ok(bytes)
}

fn verify_archive_digest(url: &Url, expected: &[u8; 32], bytes: &[u8]) -> Result<(), String> {
    let actual = Sha256::digest(bytes);
    if actual.as_slice() != expected {
        return Err(format!("SHA-256 mismatch for plugin archive {url}"));
    }
    Ok(())
}

fn publish_cached_file(destination: &Path, bytes: &[u8], context: &str) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("{context} cache path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create {context} cache {}: {error}",
            parent.display()
        )
    })?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let staging = parent.join(format!(
        ".{}-{:x}.tmp",
        std::process::id(),
        u64::from_le_bytes(random)
    ));
    fs::write(&staging, bytes)
        .map_err(|error| format!("could not write {context} staging file: {error}"))?;
    let result = match fs::rename(&staging, destination) {
        Ok(()) => Ok(()),
        Err(_) if destination.is_file() => Ok(()),
        Err(error) => Err(format!("could not publish {context}: {error}")),
    };
    let _ = fs::remove_file(staging);
    result
}

fn verify_cached_archive(destination: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "plugin archive cache path has no parent".to_string())?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let staging = parent.join(format!(
        ".verify-{}-{:x}.tmp",
        std::process::id(),
        u64::from_le_bytes(random)
    ));
    fs::create_dir(&staging).map_err(|error| {
        format!("could not create plugin archive verification directory: {error}")
    })?;
    let result = extract_archive(bytes, &staging).and_then(|()| {
        let expected = read_directory_inventory(&staging)?;
        let actual = read_directory_inventory(destination)?;
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "cached plugin archive does not match its SHA-256 source: {}",
                destination.display()
            ))
        }
    });
    let _ = fs::remove_dir_all(staging);
    result
}

fn read_directory_inventory(root: &Path) -> Result<BTreeMap<PathBuf, CachedEntry>, String> {
    let mut inventory = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    let mut bytes_read = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("could not read plugin archive cache: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("could not read plugin archive entry: {error}"))?;
            if inventory.len() >= MAX_ARCHIVE_ENTRIES {
                return Err("plugin archive cache exceeds entry limit".into());
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "plugin archive entry escaped its root".to_string())?
                .to_path_buf();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("could not inspect plugin archive entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "plugin archive cache contains a symlink: {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                inventory.insert(relative, CachedEntry::Directory);
                pending.push(path);
            } else if metadata.is_file() && metadata.len() <= MAX_FILE_BYTES {
                bytes_read = bytes_read
                    .checked_add(metadata.len())
                    .filter(|total| *total <= MAX_EXPANDED_BYTES)
                    .ok_or_else(|| {
                        "plugin archive cache exceeds expanded size limit".to_string()
                    })?;
                let bytes = fs::read(&path)
                    .map_err(|error| format!("could not read plugin archive entry: {error}"))?;
                inventory.insert(relative, CachedEntry::File(bytes));
            } else {
                return Err(format!(
                    "plugin archive cache contains an invalid entry: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(inventory)
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
        let text = std::str::from_utf8(pair)
            .map_err(|error| format!("could not parse plugin archive sha256: {error}"))?;
        digest[index] = u8::from_str_radix(text, 16)
            .map_err(|error| format!("could not parse plugin archive sha256: {error}"))?;
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
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::XGlobalHeader {
            if entry.size() > MAX_FILE_BYTES {
                return Err("plugin archive metadata exceeds size limits".into());
            }
            continue;
        }
        let path = validate_archive_path(&entry.path().map_err(|error| error.to_string())?)?;
        if !paths.insert(path.clone()) {
            return Err(format!("duplicate plugin archive path {}", path.display()));
        }
        let output = destination.join(&path);
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

    struct TestRepository {
        directory: tempfile::TempDir,
    }

    impl TestRepository {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repository = Self { directory };
            repository.git(&["init", "--quiet"]);
            repository.git(&["config", "user.name", "Kit Test"]);
            repository.git(&["config", "user.email", "kit@example.invalid"]);
            repository
        }

        fn path(&self) -> &Path {
            self.directory.path()
        }

        fn command(&self, args: &[&str]) -> Command {
            let hooks = self.path().join(".test-hooks");
            fs::create_dir_all(&hooks).unwrap();
            let mut command = Command::new("git");
            command
                .arg("-c")
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args(args)
                .current_dir(self.path())
                .env("GIT_TERMINAL_PROMPT", "0");
            command
        }

        fn git(&self, args: &[&str]) -> String {
            let output = self.command(args).output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn git_input(&self, args: &[&str], input: &[u8]) -> String {
            let mut command = self.command(args);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn commit_file(&self, path: &str, body: &[u8], message: &str) -> String {
            let path = self.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "-m", message]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    #[derive(Default)]
    struct TimeoutRunner;

    struct OutputLimitRunner;

    struct RewrittenUrlRunner;

    struct RecordingRunner {
        inner: SystemGitRunner,
        calls: std::sync::Mutex<Vec<Vec<OsString>>>,
    }

    impl Default for RecordingRunner {
        fn default() -> Self {
            Self {
                inner: SystemGitRunner::default(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl GitRunner for TimeoutRunner {
        fn run(&self, _request: GitRunRequest<'_>) -> Result<Vec<u8>, GitFailure> {
            Err(GitFailure::Timeout)
        }

        fn archive(
            &self,
            _request: GitRunRequest<'_>,
            _destination: &Path,
        ) -> Result<(), GitFailure> {
            Err(GitFailure::Timeout)
        }
    }

    impl GitRunner for OutputLimitRunner {
        fn run(&self, _request: GitRunRequest<'_>) -> Result<Vec<u8>, GitFailure> {
            Err(GitFailure::OutputLimit)
        }

        fn archive(
            &self,
            _request: GitRunRequest<'_>,
            _destination: &Path,
        ) -> Result<(), GitFailure> {
            Err(GitFailure::OutputLimit)
        }
    }

    impl GitRunner for RewrittenUrlRunner {
        fn run(&self, _request: GitRunRequest<'_>) -> Result<Vec<u8>, GitFailure> {
            Ok(b"https://rewritten.example/repository.git\n".to_vec())
        }

        fn archive(
            &self,
            _request: GitRunRequest<'_>,
            _destination: &Path,
        ) -> Result<(), GitFailure> {
            Err(GitFailure::Unavailable(io::ErrorKind::Unsupported))
        }
    }

    impl GitRunner for RecordingRunner {
        fn run(&self, request: GitRunRequest<'_>) -> Result<Vec<u8>, GitFailure> {
            self.calls.lock().unwrap().push(request.args.to_vec());
            self.inner.run(request)
        }

        fn archive(
            &self,
            request: GitRunRequest<'_>,
            destination: &Path,
        ) -> Result<(), GitFailure> {
            self.calls.lock().unwrap().push(request.args.to_vec());
            self.inner.archive(request, destination)
        }
    }

    #[test]
    fn parses_plugin_sources_with_unknown_fields() {
        let path: PluginConfig = toml::from_str("source = 'path'\npath = './plugin'").unwrap();
        assert!(matches!(path, PluginConfig::Path { .. }));
        let archive: PluginConfig = toml::from_str(&format!(
            "source = 'archive'\nurl = 'https://example.com/plugin.zip'\nsha256 = '{}'",
            "ab".repeat(32)
        ))
        .unwrap();
        assert!(matches!(archive, PluginConfig::Archive { .. }));
        let git: PluginConfig = toml::from_str(
            "source = 'git'\nurl = 'https://plugins.example/repository.git'\nrev = 'v1'\nsubdir = 'plugins/example'",
        )
        .unwrap();
        assert!(matches!(git, PluginConfig::Git { rev: Some(_), .. }));
        let git_default: PluginConfig =
            toml::from_str("source = 'git'\nurl = 'https://plugins.example/repository.git'")
                .unwrap();
        assert!(matches!(git_default, PluginConfig::Git { rev: None, .. }));
        assert!(matches!(
            toml::from_str::<PluginConfig>("source = 'path'\npath = '.'\nfuture_option = true"),
            Ok(PluginConfig::Path { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_directory_symlink_mutation_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("plugin");
        let original = package.join("skills/live-skill");
        fs::create_dir_all(&original).unwrap();
        fs::write(original.join("SKILL.md"), "safe").unwrap();
        let canonical_package = package.canonicalize().unwrap();
        let source = canonical_package.join("skills/live-skill/SKILL.md");
        assert!(
            source
                .canonicalize()
                .unwrap()
                .starts_with(&canonical_package)
        );

        let moved = package.join("skills-original");
        fs::rename(package.join("skills"), &moved).unwrap();
        let outside = directory.path().join("outside/live-skill");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("SKILL.md"), "outside").unwrap();
        symlink(outside.parent().unwrap(), package.join("skills")).unwrap();

        let error = open_snapshot_file(&canonical_package, &source).unwrap_err();
        assert!(error.contains("without following links"));
    }

    #[tokio::test]
    async fn poisoned_unpublished_skill_generation_is_rebuilt() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let package = directory.path().join("plugin");
        let skill = package.join("skills/live-skill");
        fs::create_dir_all(&skill).unwrap();
        fs::write(package.join("plugin.json"), MANIFEST).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: live-skill\ndescription: Live skill.\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            &config,
            format!(
                "[plugins.live]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();
        let runtime = PluginRuntime::new(
            config,
            directory.path().to_path_buf(),
            directory.path().join("cache"),
            directory.path().join("data"),
            ResolvedPlugins::default(),
        );
        let staged = runtime.stage().await.unwrap();
        let generation = staged.resolved.package_roots[0]
            .parent()
            .expect("snapshot package has a generation root");
        let cached_skill = staged.resolved.skill_directories[0].join("SKILL.md");
        assert!(fs::write(&cached_skill, "blocked mutation").is_err());
        make_tree_writable(generation);
        fs::write(&cached_skill, "poisoned").unwrap();

        let rebuilt = runtime.stage().await.unwrap();
        assert_eq!(rebuilt.resolved.skills[0].body, "body");
        assert!(!rebuilt.resolved.skills[0].body.contains("poisoned"));
        assert!(runtime.snapshot().skill_directories.is_empty());
    }

    #[tokio::test]
    async fn live_runtime_stages_add_remove_and_retains_last_valid_generation() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let package = directory.path().join("plugin");
        let skill = package.join("skills/live-skill");
        fs::create_dir_all(&skill).unwrap();
        fs::write(package.join("plugin.json"), MANIFEST).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: live-skill\ndescription: Live skill.\n---\nbody\n",
        )
        .unwrap();
        fs::write(&config, "").unwrap();
        let runtime = PluginRuntime::load(
            config.clone(),
            directory.path().to_path_buf(),
            directory.path().join("cache"),
            directory.path().join("data"),
        )
        .await
        .unwrap();
        let mcp = crate::tools::mcp::connect_dynamic(
            None::<&Path>,
            runtime.clone(),
            false,
            crate::tools::mcp::CredentialStorage::Memory,
        )
        .await
        .unwrap();
        assert!(runtime.snapshot().package_roots.is_empty());

        fs::write(
            &config,
            format!(
                "[plugins.live]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();
        mcp.refresh().await.unwrap();
        assert_eq!(runtime.snapshot().package_roots.len(), 1);
        assert_eq!(runtime.snapshot().skill_directories.len(), 1);
        let first_skill = runtime.snapshot().skill_directories[0].join("SKILL.md");
        assert!(fs::read_to_string(&first_skill).unwrap().contains("body"));

        write!(
            fs::File::create(package.join("mcp.json")).unwrap(),
            "{{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json\",\"mcpServers\":{{}},\"extra\":true}}"
        )
        .unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: live-skill\ndescription: Changed skill.\n---\nchanged body\n",
        )
        .unwrap();
        assert!(mcp.refresh().await.is_err());
        assert_eq!(
            runtime.snapshot().skill_directories[0].join("SKILL.md"),
            first_skill
        );
        assert!(
            !fs::read_to_string(&first_skill)
                .unwrap()
                .contains("changed body")
        );

        fs::remove_file(package.join("mcp.json")).unwrap();
        mcp.refresh().await.unwrap();
        assert_ne!(
            runtime.snapshot().skill_directories[0].join("SKILL.md"),
            first_skill
        );
        assert!(
            fs::read_to_string(runtime.snapshot().skill_directories[0].join("SKILL.md"))
                .unwrap()
                .contains("changed body")
        );

        fs::write(&config, "[plugins.live\n").unwrap();
        let error = mcp.refresh().await.unwrap_err();
        assert!(error.contains("invalid plugin config"));
        assert_eq!(runtime.snapshot().package_roots.len(), 1);

        fs::write(&config, "").unwrap();
        mcp.refresh().await.unwrap();
        assert!(runtime.snapshot().package_roots.is_empty());
        assert!(runtime.snapshot().skill_directories.is_empty());
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

    #[test]
    fn validates_git_urls_revisions_and_portable_subdirs() {
        assert!(validate_git_url("https://plugins.example/repository.git").is_ok());
        assert!(validate_git_url("https://plugins.example/repository=x.git").is_ok());
        for invalid in [
            "http://plugins.example/repository.git",
            "/repository.git",
            "https://user:secret@plugins.example/repository.git",
            "https://plugins.example/repository.git?token=secret",
            "https://plugins.example/repository.git#main",
        ] {
            let error = validate_git_url(invalid).unwrap_err();
            assert!(!error.contains("secret"));
        }
        assert_eq!(
            validate_git_revision(&"A1".repeat(20)).unwrap(),
            GitRevision::Commit("a1".repeat(20))
        );
        assert_eq!(
            validate_git_revision("v1.2.3").unwrap(),
            GitRevision::Ref("v1.2.3".into())
        );
        assert_eq!(
            validate_git_revision("abc123").unwrap(),
            GitRevision::Ref("abc123".into())
        );
        assert_eq!(
            validate_git_revision("refs/tags/abc123").unwrap(),
            GitRevision::Ref("refs/tags/abc123".into())
        );
        assert_eq!(
            validate_git_revision("HEAD").unwrap(),
            GitRevision::DefaultBranch
        );
        assert_eq!(
            validate_git_revision("main").unwrap(),
            GitRevision::Ref("main".into())
        );
        assert_eq!(
            validate_git_revision("refs/heads/main").unwrap(),
            GitRevision::Ref("refs/heads/main".into())
        );
        assert_eq!(
            validate_git_revision("refs/tags/releases/v1").unwrap(),
            GitRevision::Ref("refs/tags/releases/v1".into())
        );
        for invalid in [
            "+refs/tags/v1:refs/tags/pwned",
            "-v1",
            "v1^{}",
            "v1..v2",
            "refs/tags/.hidden",
            "refs/tags/a.lock",
        ] {
            assert!(
                validate_git_revision(invalid).is_err(),
                "accepted {invalid}"
            );
        }
        let args = hardened_git_args(
            GitProtocol::Https,
            Path::new("hooks"),
            Path::new("attributes"),
            &[],
        );
        let config = hardened_git_config(Some("https://plugins.example/repository=x.git"));
        assert!(config.contains(&(
            OsString::from("http.https://plugins.example/repository=x.git.sslVerify"),
            OsString::from("true")
        )));
        assert!(config.contains(&(
            OsString::from("http.https://plugins.example/repository=x.git.followRedirects"),
            OsString::from("false")
        )));
        assert!(config.contains(&(OsString::from("core.askPass"), OsString::new())));
        assert!(config.contains(&(
            OsString::from("credential.interactive"),
            OsString::from("false")
        )));
        assert!(args.contains(&OsString::from("core.attributesFile=attributes")));
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("credential.helper="))
        );

        assert!(validate_git_subdir("plugins/reviewer").is_ok());
        for invalid in [
            "../escape",
            "./plugin",
            "plugin/./nested",
            "plugin//nested",
            "plugin/nested/",
            "CON/file",
            "COM¹/file",
            "COM²/file",
            "COM³/file",
            "LPT¹/file",
            "LPT²/file",
            "LPT³.txt/file",
            "trailing.",
            "a\\b",
            " :(glob)",
        ] {
            assert!(validate_git_subdir(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn system_git_runner_applies_url_config_with_equals_in_path() {
        let directory = tempfile::tempdir().unwrap();
        let remote = "https://plugins.example/repository=x.git";
        let query = format!("http.{remote}.sslVerify");
        let command_args = [
            OsStr::new("config"),
            OsStr::new("--get"),
            OsStr::new(&query),
        ];
        let args = hardened_git_args(
            GitProtocol::Https,
            directory.path(),
            directory.path(),
            &command_args,
        );
        let config = hardened_git_config(Some(remote));
        let output = SystemGitRunner {
            timeout: Duration::from_secs(5),
        }
        .run(GitRunRequest {
            cwd: directory.path(),
            args: &args,
            config: &config,
            stdout_limit: MAX_GIT_DIAGNOSTIC_BYTES,
            stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
            object_store_limit: None,
        })
        .unwrap();

        assert_eq!(String::from_utf8(output).unwrap().trim(), "true");
    }

    #[cfg(unix)]
    #[test]
    fn system_git_runner_disables_configured_askpass_probe() {
        use std::os::unix::fs::PermissionsExt as _;

        let repository = TestRepository::new();
        let marker = repository.path().join("askpass-called");
        let probe = repository.path().join("askpass-probe.sh");
        fs::write(
            &probe,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nprintf 'secret\n'\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).unwrap();
        repository.git(&["config", "core.askPass", probe.to_str().unwrap()]);

        repository.git_input(
            &["credential", "fill"],
            b"protocol=https\nhost=example.invalid\nusername=kit\n\n",
        );
        assert!(marker.is_file(), "askpass test probe was not executable");
        fs::remove_file(&marker).unwrap();

        let alias = OsString::from(
            "alias.askpass-probe=!printf 'protocol=https\nhost=example.invalid\nusername=kit\n\n' | git credential fill",
        );
        let args = vec![OsString::from("-c"), alias, OsString::from("askpass-probe")];
        let config = hardened_git_config(None);
        let result = SystemGitRunner {
            timeout: Duration::from_secs(5),
        }
        .run(GitRunRequest {
            cwd: repository.path(),
            args: &args,
            config: &config,
            stdout_limit: MAX_GIT_DIAGNOSTIC_BYTES,
            stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
            object_store_limit: None,
        });

        assert!(matches!(result, Err(GitFailure::Exit(_))));
        assert!(
            !marker.exists(),
            "configured core.askPass escaped SystemGitRunner controls"
        );
    }

    #[test]
    fn system_git_runner_hard_bounds_pipes_and_fails_closed_on_object_scan() {
        let directory = tempfile::tempdir().unwrap();
        let runner = SystemGitRunner {
            timeout: Duration::from_secs(5),
        };

        let version_args = vec![OsString::from("--version")];
        assert_eq!(
            runner.run(GitRunRequest {
                cwd: directory.path(),
                args: &version_args,
                config: &[],
                stdout_limit: 1,
                stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                object_store_limit: None,
            }),
            Err(GitFailure::OutputLimit)
        );

        let invalid_args = vec![OsString::from("definitely-not-a-git-command")];
        assert_eq!(
            runner.run(GitRunRequest {
                cwd: directory.path(),
                args: &invalid_args,
                config: &[],
                stdout_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                stderr_limit: 1,
                object_store_limit: None,
            }),
            Err(GitFailure::OutputLimit)
        );

        let repository = TestRepository::new();
        repository.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        let archive_destination = tempfile::tempdir().unwrap();
        let archive_args = vec![
            OsString::from("archive"),
            OsString::from("--format=tar"),
            OsString::from("HEAD"),
        ];
        assert_eq!(
            runner.archive(
                GitRunRequest {
                    cwd: repository.path(),
                    args: &archive_args,
                    config: &[],
                    stdout_limit: 64,
                    stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                    object_store_limit: None,
                },
                archive_destination.path(),
            ),
            Err(GitFailure::OutputLimit)
        );

        let counted_objects = directory.path().join("counted-objects");
        fs::create_dir(&counted_objects).unwrap();
        fs::write(counted_objects.join("one"), b"one").unwrap();
        fs::write(counted_objects.join("two"), b"two").unwrap();
        assert_eq!(
            git_object_store_size_with_limit(&counted_objects, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let missing_objects = directory.path().join("missing-objects");
        assert!(matches!(
            runner.run(GitRunRequest {
                cwd: directory.path(),
                args: &version_args,
                config: &[],
                stdout_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                object_store_limit: Some(&missing_objects),
            }),
            Err(GitFailure::ObjectStoreInspection(io::ErrorKind::NotFound))
        ));
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".git-stderr")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn system_git_runner_timeout_terminates_descendants_and_joins_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let alias = OsString::from(format!(
            "alias.spawn=!sleep 30 & echo $! > {}; wait",
            pid_file.display()
        ));
        let args = vec![OsString::from("-c"), alias, OsString::from("spawn")];
        let runner = SystemGitRunner {
            timeout: Duration::from_millis(300),
        };
        assert_eq!(
            runner.run(GitRunRequest {
                cwd: directory.path(),
                args: &args,
                config: &[],
                stdout_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                object_store_limit: None,
            }),
            Err(GitFailure::Timeout)
        );

        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let mut exists = true;
        for _ in 0..100 {
            let result = unsafe { libc::kill(pid, 0) };
            exists = result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
            if !exists {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!exists, "Git descendant {pid} survived runner cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn system_git_runner_times_out_during_archive_extraction_and_joins_threads() {
        let directory = tempfile::tempdir().unwrap();
        let mut partial_tar = tar_with_file("plugin.json", b"{}");
        partial_tar.truncate(1024);
        let partial_tar_path = directory.path().join("partial.tar");
        fs::write(&partial_tar_path, partial_tar).unwrap();
        let alias = OsString::from(format!(
            "alias.slow=!cat {}; sleep 30",
            partial_tar_path.display()
        ));
        let args = vec![OsString::from("-c"), alias, OsString::from("slow")];
        let destination = tempfile::tempdir().unwrap();
        let runner = SystemGitRunner {
            timeout: Duration::from_millis(300),
        };
        let started = Instant::now();
        assert_eq!(
            runner.archive(
                GitRunRequest {
                    cwd: directory.path(),
                    args: &args,
                    config: &[],
                    stdout_limit: MAX_TAR_STREAM_BYTES,
                    stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
                    object_store_limit: None,
                },
                destination.path(),
            ),
            Err(GitFailure::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            fs::read(destination.path().join("plugin.json")).unwrap(),
            b"{}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_git_runner_fails_closed_when_live_object_store_becomes_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let objects = directory.path().join("objects");
        fs::create_dir(&objects).unwrap();
        let objects_for_thread = objects.clone();
        let invalidator = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            fs::remove_dir_all(&objects_for_thread).unwrap();
            fs::write(objects_for_thread, b"not a directory").unwrap();
        });
        let args = vec![
            OsString::from("-c"),
            OsString::from("alias.wait=!sleep 30"),
            OsString::from("wait"),
        ];
        let runner = SystemGitRunner {
            timeout: Duration::from_secs(5),
        };
        let started = Instant::now();
        let result = runner.run(GitRunRequest {
            cwd: directory.path(),
            args: &args,
            config: &[],
            stdout_limit: MAX_GIT_DIAGNOSTIC_BYTES,
            stderr_limit: MAX_GIT_DIAGNOSTIC_BYTES,
            object_store_limit: Some(&objects),
        });
        invalidator.join().unwrap();
        assert_eq!(
            result,
            Err(GitFailure::ObjectStoreInspection(
                io::ErrorKind::InvalidData
            ))
        );
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn rejects_remote_bytes_and_fetch_head_in_non_object_git_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let git_dir = directory.path();
        fs::create_dir(git_dir.join("objects")).unwrap();
        let remote = OsStr::new("https://plugins.example/private.git");
        fs::write(
            git_dir.join("objects/ignored"),
            b"https://plugins.example/private.git",
        )
        .unwrap();
        enforce_git_staging_metadata(git_dir, remote).unwrap();

        fs::write(
            git_dir.join("config-leak"),
            b"https://plugins.example/private.git",
        )
        .unwrap();
        assert!(
            enforce_git_staging_metadata(git_dir, remote)
                .unwrap_err()
                .contains("configured remote")
        );
        fs::remove_file(git_dir.join("config-leak")).unwrap();

        fs::write(git_dir.join("FETCH_HEAD"), b"forbidden").unwrap();
        assert!(
            enforce_git_staging_metadata(git_dir, remote)
                .unwrap_err()
                .contains("FETCH_HEAD")
        );
    }

    #[test]
    fn rejects_rewritten_effective_url_before_network_and_cleans_staging() {
        let cache = tempfile::tempdir().unwrap();
        let remote = "https://plugins.example/repository.git";
        let source_key = sha256_text(remote);
        let error = resolve_git_source(
            OsStr::new(remote),
            &source_key,
            &GitRevision::Commit("01".repeat(20)),
            None,
            cache.path(),
            GitProtocol::Https,
            &RewrittenUrlRunner,
        )
        .unwrap_err();
        assert!(error.contains("rewritten"));
        assert!(
            fs::read_dir(cache.path().join(GIT_CACHE_VERSION).join(source_key))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".staging-"))
        );
    }

    #[test]
    fn staging_guard_removes_current_and_safely_named_stale_directories() {
        let parent = tempfile::tempdir().unwrap();
        let stale = parent.path().join(".staging-0000000000000000");
        let unrelated = parent.path().join(".staging-not-owned");
        fs::create_dir(&stale).unwrap();
        fs::create_dir(&unrelated).unwrap();
        cleanup_stale_staging(parent.path(), ".staging-", Duration::ZERO).unwrap();
        assert!(!stale.exists());
        assert!(unrelated.exists());

        let active_path;
        {
            let staging = StagingDirectory::create(parent.path(), ".staging-").unwrap();
            active_path = staging.path().to_path_buf();
            assert!(active_path.is_dir());
        }
        assert!(!active_path.exists());
    }

    #[test]
    fn resolves_default_branch_and_main_to_commits() {
        let repository = TestRepository::new();
        let first = repository.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        repository.git(&["branch", "-M", "main"]);
        let cache = tempfile::tempdir().unwrap();

        let default_root = resolve_git_local_revision(
            repository.path(),
            GitRevision::DefaultBranch,
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        assert!(
            default_root
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&first)
        );

        let second = repository.commit_file("version.txt", b"second", "second");
        let main_root = resolve_git_local(
            repository.path(),
            "main",
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        assert!(
            main_root
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&second)
        );
        assert_eq!(fs::read(main_root.join("version.txt")).unwrap(), b"second");
    }

    #[test]
    fn resolves_git_commit_and_reuses_it_offline() {
        let repository = TestRepository::new();
        let commit = repository.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        let cache = tempfile::tempdir().unwrap();
        let root = resolve_git_local(
            repository.path(),
            &commit,
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("plugin.json")).unwrap(),
            MANIFEST
        );

        // A full commit cache hit does not invoke the now-unavailable runner.
        let root_again = resolve_git_local(
            repository.path(),
            &commit,
            None,
            cache.path(),
            &TimeoutRunner,
        )
        .unwrap();
        assert_eq!(root_again, root);

        fs::write(root.parent().unwrap().join("unexpected"), b"corrupt").unwrap();
        let error = resolve_git_local(
            repository.path(),
            &commit,
            None,
            cache.path(),
            &TimeoutRunner,
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(
            fs::read_dir(root.parent().unwrap().parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".staging-"))
        );
    }

    #[test]
    fn forces_sha1_and_preserves_only_committed_git_attributes() {
        let repository = TestRepository::new();
        repository.commit_file(
            ".gitattributes",
            b"secret.txt export-ignore\n",
            "attributes",
        );
        repository.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        let commit = repository.commit_file("secret.txt", b"secret", "secret");
        let cache = tempfile::tempdir().unwrap();
        let runner = RecordingRunner::default();
        let root =
            resolve_git_local(repository.path(), &commit, None, cache.path(), &runner).unwrap();
        assert!(root.join(".gitattributes").is_file());
        assert!(!root.join("secret.txt").exists());

        let calls = runner.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .flatten()
                .any(|arg| arg == "--object-format=sha1")
        );
        assert_eq!(
            calls
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "--get-url"))
                .count(),
            2
        );
        assert!(calls.iter().all(|args| {
            args.iter()
                .any(|arg| arg.to_string_lossy().starts_with("core.attributesFile="))
        }));
        let fetch = calls
            .iter()
            .find(|args| args.iter().any(|arg| arg == "--no-write-fetch-head"))
            .expect("fetch must suppress FETCH_HEAD");
        assert!(
            fetch
                .iter()
                .any(|arg| { arg == &OsString::from(format!("{commit}:{GIT_PRIVATE_FETCH_REF}")) })
        );
        assert!(calls.iter().any(|args| {
            args.iter()
                .any(|arg| arg == &OsString::from(format!("{GIT_PRIVATE_FETCH_REF}^{{commit}}")))
        }));
        assert!(
            calls
                .iter()
                .flatten()
                .all(|arg| arg != OsStr::new("FETCH_HEAD^{commit}"))
        );
    }

    #[test]
    fn resolves_fresh_lightweight_and_annotated_tags() {
        let repository = TestRepository::new();
        let first = repository.commit_file("plugin.json", MANIFEST.as_bytes(), "first");
        repository.git(&["tag", "stable", &first]);
        repository.git(&["tag", "-a", "annotated", "-m", "release", &first]);
        let cache = tempfile::tempdir().unwrap();
        let first_root = resolve_git_local(
            repository.path(),
            "stable",
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        resolve_git_local(
            repository.path(),
            "refs/tags/annotated",
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();

        let second = repository.commit_file("version.txt", b"second", "second");
        repository.git(&["tag", "--force", "stable", &second]);
        let second_root = resolve_git_local(
            repository.path(),
            "refs/tags/stable",
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        assert_ne!(first_root, second_root);
        assert_eq!(
            fs::read(second_root.join("version.txt")).unwrap(),
            b"second"
        );
    }

    #[test]
    fn resolves_git_subdir_with_literal_pathspecs() {
        let repository = TestRepository::new();
        repository.commit_file("plugin.json", MANIFEST.as_bytes(), "root plugin");
        let commit = repository.commit_file(
            "packages/reviewer/plugin.json",
            MANIFEST.as_bytes(),
            "nested plugin",
        );
        let cache = tempfile::tempdir().unwrap();
        let root_package = resolve_git_local(
            repository.path(),
            &commit,
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        let nested_package = resolve_git_local(
            repository.path(),
            &commit,
            Some(Path::new("packages/reviewer")),
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap();
        assert!(root_package.join("plugin.json").is_file());
        assert!(nested_package.join("plugin.json").is_file());
        assert_ne!(root_package, nested_package);
    }

    #[test]
    fn rejects_noncommit_tags_symlinks_and_submodules() {
        let noncommit = TestRepository::new();
        noncommit.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        let blob = noncommit.git(&["hash-object", "plugin.json"]);
        noncommit.git(&["tag", "blob", &blob]);
        let cache = tempfile::tempdir().unwrap();
        let error = resolve_git_local(
            noncommit.path(),
            "blob",
            None,
            cache.path(),
            &SystemGitRunner::default(),
        )
        .unwrap_err();
        assert!(error.contains("not a commit") || error.contains("commit verification"));

        let symlink = TestRepository::new();
        symlink.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        let link_blob = symlink.git_input(&["hash-object", "-w", "--stdin"], b"target");
        symlink.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "120000",
            &link_blob,
            "link",
        ]);
        symlink.git(&["commit", "--quiet", "-m", "symlink"]);
        let symlink_commit = symlink.git(&["rev-parse", "HEAD"]);
        let error = resolve_git_local(
            symlink.path(),
            &symlink_commit,
            None,
            tempfile::tempdir().unwrap().path(),
            &SystemGitRunner::default(),
        )
        .unwrap_err();
        assert!(error.contains("symbolic links"));

        let submodule = TestRepository::new();
        let parent = submodule.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        submodule.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &parent,
            "nested",
        ]);
        submodule.git(&["commit", "--quiet", "-m", "submodule"]);
        let submodule_commit = submodule.git(&["rev-parse", "HEAD"]);
        let error = resolve_git_local(
            submodule.path(),
            &submodule_commit,
            None,
            tempfile::tempdir().unwrap().path(),
            &SystemGitRunner::default(),
        )
        .unwrap_err();
        assert!(error.contains("submodules"));
    }

    #[test]
    fn enforces_git_object_size_limits() {
        let too_large = format!(
            "100644 blob {} {}\tlarge.bin\0",
            "01".repeat(20),
            MAX_FILE_BYTES + 1
        );
        assert!(
            validate_git_tree(too_large.as_bytes())
                .unwrap_err()
                .contains("size limits")
        );

        let store = tempfile::tempdir().unwrap();
        fs::create_dir(store.path().join("pack")).unwrap();
        File::create(store.path().join("pack/oversized.pack"))
            .unwrap()
            .set_len(MAX_EXPANDED_BYTES + 1)
            .unwrap();
        assert!(
            enforce_git_object_store_limit(store.path())
                .unwrap_err()
                .contains("size limit")
        );
    }

    #[test]
    fn concurrent_git_publishers_validate_the_winner() {
        let repository = TestRepository::new();
        let commit = repository.commit_file("plugin.json", MANIFEST.as_bytes(), "plugin");
        let repository = repository.path().to_path_buf();
        let cache = tempfile::tempdir().unwrap();
        let cache_path = cache.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let repository = repository.clone();
                let commit = commit.clone();
                let cache_path = cache_path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    resolve_git_local(
                        &repository,
                        &commit,
                        None,
                        &cache_path,
                        &SystemGitRunner::default(),
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let roots = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(roots[0], roots[1]);
        assert!(roots[0].join("plugin.json").is_file());
    }

    #[test]
    fn git_timeout_and_output_errors_redact_the_remote() {
        let url = validate_git_url("https://plugins.example/private/opaque-secret.git").unwrap();
        let source_key = sha256_text(url.as_str());
        for (runner, expected) in [
            (&TimeoutRunner as &dyn GitRunner, "timed out"),
            (&OutputLimitRunner as &dyn GitRunner, "output limit"),
        ] {
            let cache = tempfile::tempdir().unwrap();
            let error = resolve_git_source(
                OsStr::new(url.as_str()),
                &source_key,
                &GitRevision::Ref("refs/tags/stable".into()),
                None,
                cache.path(),
                GitProtocol::Https,
                runner,
            )
            .unwrap_err();
            assert!(error.contains(expected));
            assert!(!error.contains("opaque-secret"));
            assert!(!error.contains(url.as_str()));
        }
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
    fn extracts_tar_with_global_pax_header() {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut pax = tar::Header::new_gnu();
            pax.set_entry_type(tar::EntryType::XGlobalHeader);
            pax.set_size(0);
            pax.set_mode(0o644);
            pax.set_cksum();
            builder
                .append_data(&mut pax, "pax_global_header", io::empty())
                .unwrap();

            let body = b"{}";
            let mut file = tar::Header::new_gnu();
            file.set_size(body.len() as u64);
            file.set_mode(0o644);
            file.set_cksum();
            builder
                .append_data(&mut file, "plugin/plugin.json", body.as_slice())
                .unwrap();
            builder.finish().unwrap();
        }

        let destination = tempfile::tempdir().unwrap();
        extract_archive(&bytes, destination.path()).unwrap();
        assert!(destination.path().join("plugin/plugin.json").is_file());
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
        fs::write(root.join("plugin.json"), "poisoned").unwrap();
        assert!(
            resolve_archive(&url, &digest, None, cache.path())
                .unwrap_err()
                .contains("does not match its SHA-256 source")
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
