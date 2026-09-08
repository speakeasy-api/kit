//! Durable, session-authorized immutable image snapshots. File values are JSON
//! capabilities only within the session that imported them, never filesystem paths.
use std::{
    collections::HashMap,
    io::{Cursor, Read as _, Write as _},
    path::{Path, PathBuf},
};

use agentkit_core::{DataRef, Modality, Part, TurnCancellation};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::resilient_fs as fs;

mod operations;
pub(crate) use operations::{Anchor, AspectRatio, Fit, Transform};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 4096;
const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DELIVERY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DELIVERY_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_IMAGES: usize = 8;
pub(crate) const MAX_LABEL_BYTES: usize = 4 * 1024;
const MAX_OCCURRENCES: usize = 64;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 64;
const MAGIC: &[u8; 8] = b"KITFILE1";

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileReference {
    #[serde(rename = "$kit")]
    marker: String,
    version: u32,
    id: String,
    name: String,
    mime_type: String,
    size_bytes: u64,
    image: ImageDimensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageDimensions {
    width: u32,
    height: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header {
    file: FileReference,
    digest: String,
}

#[derive(Clone)]
pub(crate) struct FileStore {
    base: PathBuf,
}

/// Unique cleanup owner for a fresh inherited authority directory. It follows
/// pending transcript creation into response publication; only commit retains it.
pub(crate) struct InheritedAuthority {
    path: Option<PathBuf>,
    quarantine: PathBuf,
    identity: fs::FileIdentity,
    session: String,
}

impl InheritedAuthority {
    pub(crate) fn is_for_session(&self, session: &str) -> bool {
        self.session == session
    }

    pub(crate) fn commit(mut self) {
        self.path = None;
    }
}

impl Drop for InheritedAuthority {
    fn drop(&mut self) {
        let Some(path) = &self.path else { return };
        let identity = |path: &Path| fs::Backend::identity(&fs::DiskBackend, path, false);
        if identity(path).ok().flatten() != Some(self.identity) {
            return;
        }
        // Detach before recursive removal. Verify again after the atomic move,
        // so a replaced directory is never deleted, even across a pathname race.
        if rename_no_replace(path, &self.quarantine).is_err() {
            return;
        }
        if identity(&self.quarantine).ok().flatten() != Some(self.identity) {
            let _ = rename_no_replace(&self.quarantine, path);
            return;
        }
        let _ = fs::remove_dir_all(&self.quarantine);
        if let Some(parent) = path.parent() {
            let _ = fs::sync_directory(parent);
        }
    }
}

impl FileStore {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            base: crate::artifacts::base(root).with_file_name("files"),
        }
    }

    fn session_directory(&self, session: &str) -> PathBuf {
        self.base
            .join(blake3::hash(session.as_bytes()).to_hex().as_str())
    }

    pub(crate) fn import(
        &self,
        session: &str,
        path: &Path,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<FileReference> {
        check_cancelled(cancellation)?;
        // Check before opening as well as on the opened descriptor. Nonblocking
        // open on Unix prevents a replacement FIFO from blocking between checks.
        if !std::fs::metadata(path).map_err(display)?.is_file() {
            return Err("read_file requires a regular local file".into());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path).map_err(display)?;
        let metadata = file.metadata().map_err(display)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err("read_file requires a regular file no larger than 8 MiB".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(display)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("read_file image must contain 1 byte to 8 MiB".into());
        }
        check_cancelled(cancellation)?;
        let (mime_type, image) = inspect_image(&bytes)?;
        check_cancelled(cancellation)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| valid_name(name))
            .ok_or(
                "file name must be nonempty UTF-8, at most 255 bytes, without control characters",
            )?
            .to_owned();
        self.publish(session, bytes, name, mime_type, image, cancellation)
    }

    /// Imports actual native image bytes, never a URI or descriptor surrogate.
    pub(crate) fn import_bytes(
        &self,
        session: &str,
        name: &str,
        mime_type: &str,
        bytes: &[u8],
        cancellation: Option<&TurnCancellation>,
    ) -> Result<FileReference> {
        check_cancelled(cancellation)?;
        if !valid_name(name) || bytes.is_empty() || bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("invalid image name or byte budget (1 byte to 8 MiB)".into());
        }
        let (actual_mime, dimensions) = inspect_image(bytes)?;
        if mime_type != actual_mime {
            return Err("declared image MIME type does not match its bytes".into());
        }
        self.publish(
            session,
            bytes.to_vec(),
            name.into(),
            actual_mime,
            dimensions,
            cancellation,
        )
    }

    /// Resolves only the caller's authorized immutable snapshot for ACP input.
    pub(crate) fn attachment_image(
        &self,
        session: &str,
        reference: &FileReference,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<Part> {
        check_cancelled(cancellation)?;
        let bytes = self.resolve(session, reference)?;
        check_cancelled(cancellation)?;
        Ok(Part::media(
            Modality::Image,
            reference.mime_type.clone(),
            DataRef::InlineBytes(bytes),
        ))
    }

    /// Explicit durable replication, preserving identity without global lookup.
    /// Existing destinations must match metadata and payload; never clobber.
    pub(crate) fn grant_to(
        &self,
        session: &str,
        reference: &FileReference,
        destination_store: &Self,
        destination_session: &str,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<FileReference> {
        check_cancelled(cancellation)?;
        let bytes = self.resolve(session, reference)?;
        check_cancelled(cancellation)?;
        let directory = destination_store.session_directory(destination_session);
        let destination = directory.join(&reference.id);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                // Existence only selects this branch; source and destination
                // must both resolve with matching metadata, digest and bytes.
                // Repair any outstanding directory durability after a prior
                // publication attempt, without allocating a new disk object.
                if destination_store.resolve(destination_session, reference)? != bytes {
                    return Err("existing managed grant has conflicting bytes".into());
                }
                check_cancelled(cancellation)?;
                fs::sync_directory(&directory).map_err(display)?;
                fs::require_disk(&directory).map_err(display)?;
                fs::sync_directory(&destination_store.base).map_err(display)?;
                fs::require_disk(&destination).map_err(display)?;
                check_cancelled(cancellation)?;
                return Ok(reference.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(display(error)),
        }
        fs::create_private_dir_all(&directory).map_err(display)?;
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(display)?;
        let staging_session = format!("grant-{}", blake3::Hash::from_bytes(random).to_hex());
        let staging_directory = destination_store.session_directory(&staging_session);
        let staged = staging_directory.join(&reference.id);
        let result = (|| {
            destination_store.write_snapshot(
                &staging_session,
                reference.clone(),
                &bytes,
                cancellation,
            )?;
            check_cancelled(cancellation)?;
            // Atomic exclusive rename preserves nlink == 1 throughout: readers
            // and restart never observe a multiply linked published envelope.
            match rename_no_replace(&staged, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(display(error)),
            }
            if destination_store.resolve(destination_session, reference)? != bytes {
                return Err("existing managed grant has conflicting bytes".into());
            }
            fs::sync_directory(&staging_directory).map_err(display)?;
            fs::require_disk(&staging_directory).map_err(display)?;
            fs::sync_directory(&directory).map_err(display)?;
            fs::require_disk(&directory).map_err(display)?;
            fs::sync_directory(&destination_store.base).map_err(display)?;
            fs::require_disk(&destination).map_err(display)?;
            Ok(reference.clone())
        })();
        // A crash can leave unreachable staging bytes, never partial grants.
        let _ = fs::remove_file(&staged);
        let _ = std::fs::remove_dir(&staging_directory);
        result
    }

    /// Prepare a fresh inherited set. The caller must retain its cleanup owner
    /// until session/response publication. Commit is infallible and does no I/O;
    /// rollback must happen outside shared writer/registry locks.
    pub(crate) fn prepare_inheritance(
        &self,
        source: &str,
        destination: &str,
    ) -> Result<Option<InheritedAuthority>> {
        let target = self.session_directory(destination);
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err("fork destination already has managed file authority".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(display(error)),
        }
        let directory = self.session_directory(source);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(display(error)),
        };
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(display)?;
        let staging_session = format!("fork-{}", blake3::Hash::from_bytes(random).to_hex());
        let staging_directory = self.session_directory(&staging_session);
        fs::create_private_dir_all(&staging_directory).map_err(display)?;
        let identity = fs::Backend::identity(&fs::DiskBackend, &staging_directory, false)
            .map_err(display)?
            .ok_or("inherited authority requires native directory identity")?;
        let mut authority = InheritedAuthority {
            path: Some(staging_directory.clone()),
            quarantine: staging_directory.with_extension("rollback"),
            identity,
            session: destination.into(),
        };
        for entry in entries {
            let entry = entry.map_err(display)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("invalid managed object filename")?;
            let mut file = fs::open_beneath(&directory, Path::new(name)).map_err(display)?;
            let mut prefix = [0_u8; 12];
            file.read_exact(&mut prefix).map_err(display)?;
            let length = u32::from_le_bytes(prefix[8..12].try_into().map_err(display)?) as usize;
            if &prefix[..8] != MAGIC || length > MAX_HEADER_BYTES {
                return Err("invalid inherited managed file envelope".into());
            }
            let mut header = vec![0; length];
            file.read_exact(&mut header).map_err(display)?;
            let header: Header = serde_json::from_slice(&header).map_err(display)?;
            if header.file.id != name {
                return Err("inherited file ID does not match its storage name".into());
            }
            self.grant_to(source, &header.file, self, &staging_session, None)?;
        }
        // The preflight is not authority: even an empty destination created
        // concurrently must survive unchanged at the atomic commit boundary.
        rename_no_replace(&staging_directory, &target).map_err(display)?;
        authority.path = Some(target.clone());
        fs::sync_directory(&self.base).map_err(display)?;
        fs::require_disk(&target).map_err(display)?;
        Ok(Some(authority))
    }

    fn publish(
        &self,
        session: &str,
        bytes: Vec<u8>,
        name: String,
        mime_type: String,
        image: ImageDimensions,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<FileReference> {
        check_cancelled(cancellation)?;
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(display)?;
        let id = format!("file_{}", blake3::Hash::from_bytes(random).to_hex());
        let reference = FileReference {
            marker: "file".into(),
            version: 1,
            id,
            name,
            mime_type,
            size_bytes: bytes.len() as u64,
            image,
        };
        self.write_snapshot(session, reference, &bytes, cancellation)
    }

    fn write_snapshot(
        &self,
        session: &str,
        reference: FileReference,
        bytes: &[u8],
        cancellation: Option<&TurnCancellation>,
    ) -> Result<FileReference> {
        check_cancelled(cancellation)?;
        let header = serde_json::to_vec(&Header {
            file: reference.clone(),
            digest: blake3::hash(bytes).to_hex().to_string(),
        })
        .map_err(display)?;
        if header.len() > MAX_HEADER_BYTES {
            return Err("managed file header is too large".into());
        }
        let directory = self.session_directory(session);
        let mut durability_directories = Vec::new();
        for ancestor in directory.ancestors() {
            durability_directories.push(ancestor);
            if fs::try_exists(ancestor).map_err(display)? {
                break;
            }
        }
        fs::create_private_dir_all(&directory).map_err(display)?;
        let destination = directory.join(&reference.id);
        // No descriptor is published until all durability barriers succeed.
        // Partial and cancelled imports may leave unreachable objects, never a
        // reference that claims an in-memory-only snapshot survived a restart.
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .private(true)
            .open(&destination)
            .map_err(display)?;
        output.write_all(MAGIC).map_err(display)?;
        output
            .write_all(&(header.len() as u32).to_le_bytes())
            .map_err(display)?;
        output.write_all(&header).map_err(display)?;
        output.write_all(bytes).map_err(display)?;
        output.sync_all().map_err(display)?;
        fs::require_disk(&destination).map_err(display)?;
        // Include newly created ancestor entries, not just the object contents.
        for ancestor in durability_directories {
            fs::sync_directory(ancestor).map_err(display)?;
            fs::require_disk(ancestor).map_err(display)?;
        }
        check_cancelled(cancellation)?;
        Ok(reference)
    }

    pub(crate) fn resolve(&self, session: &str, selected: &FileReference) -> Result<Vec<u8>> {
        selected.validate()?;
        let directory = self.session_directory(session);
        // A reference must never resolve an import still retained only in the
        // resilient filesystem's volatile write-back layer.
        fs::require_disk(directory.join(&selected.id)).map_err(display)?;
        let mut file = fs::open_beneath(&directory, Path::new(&selected.id)).map_err(|error| {
            format!("managed file is missing or inaccessible in this session: {error}")
        })?;
        let length = file.metadata().map_err(display)?.len();
        if length > MAX_FILE_BYTES + MAX_HEADER_BYTES as u64 + 12 {
            return Err("managed file exceeds its storage budget".into());
        }
        let mut prefix = [0_u8; 12];
        file.read_exact(&mut prefix).map_err(display)?;
        if &prefix[..8] != MAGIC {
            return Err("unsupported managed file envelope".into());
        }
        let header_length =
            u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]) as usize;
        if header_length == 0 || header_length > MAX_HEADER_BYTES {
            return Err("invalid managed file header length".into());
        }
        let mut header = vec![0; header_length];
        file.read_exact(&mut header).map_err(display)?;
        let header: Header = serde_json::from_slice(&header).map_err(display)?;
        header.file.validate()?;
        if &header.file != selected {
            return Err("selected file metadata does not match its stored object".into());
        }
        if length != 12 + header_length as u64 + selected.size_bytes {
            return Err("managed file envelope length does not match its payload".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(display)?;
        if bytes.len() as u64 != selected.size_bytes
            || blake3::hash(&bytes).to_hex().as_str() != header.digest
        {
            return Err("managed file payload is truncated or corrupt".into());
        }
        // Digest and metadata bind the already validated immutable import. No
        // repeated pixel decode is necessary for each selection or replay.
        Ok(bytes)
    }

    pub(crate) fn selected_parts(
        &self,
        session: &str,
        value: &Value,
        cancellation: Option<&TurnCancellation>,
    ) -> Result<Vec<Part>> {
        let mut selection = Selection::default();
        selection.walk(value, String::new(), 0)?;
        let mut validated: HashMap<String, FileReference> = HashMap::new();
        let mut selected = Vec::new();
        let mut label_bytes = 0_usize;
        let mut bytes = 0_u64;
        let mut pixels = 0_u64;
        for (pointer, reference) in selection.files {
            check_cancelled(cancellation)?;
            reference.validate()?;
            if let Some(previous) = validated.get(&reference.id) {
                if previous != &reference {
                    return Err("duplicate file reference has contradictory metadata".into());
                }
                continue;
            }
            bytes += reference.size_bytes;
            pixels += reference.pixels();
            if validated.len() >= MAX_IMAGES
                || bytes > MAX_DELIVERY_BYTES
                || pixels > MAX_DELIVERY_PIXELS
            {
                return Err(
                    "selected images exceed delivery budget (8 images, 16 MiB, 32 megapixels)"
                        .into(),
                );
            }
            let label = format!(
                "Image #{} at JSON Pointer {}: {}",
                validated.len() + 1,
                serde_json::to_string(&pointer).map_err(display)?,
                reference.name
            );
            label_bytes += label.len();
            if label_bytes > MAX_LABEL_BYTES {
                return Err("selected image labels exceed the 4 KiB label budget".into());
            }
            validated.insert(reference.id.clone(), reference.clone());
            selected.push((label, reference));
        }
        // Preflight all occurrence metadata and descriptor/label budgets before
        // reading any payload. Nothing is emitted until every object resolves.
        let mut parts = Vec::new();
        for (label, reference) in selected {
            check_cancelled(cancellation)?;
            let image = self.attachment_image(session, &reference, cancellation)?;
            parts.push(Part::text(label));
            parts.push(image);
        }
        check_cancelled(cancellation)?;
        Ok(parts)
    }
}

impl FileReference {
    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        // Bound descriptor strings before deserialization copies them. A marker
        // does not make an arbitrarily large object a bounded File value.
        let object = value
            .as_object()
            .ok_or("File reference must be an object")?;
        if object.len() != 7
            || object
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| id.len() != 69)
            || !object
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(valid_name)
            || object
                .get("mime_type")
                .and_then(Value::as_str)
                .is_none_or(|mime| mime.len() > 10)
        {
            return Err("invalid managed File reference shape or string budget".into());
        }
        let reference = Self::deserialize(value).map_err(display)?;
        reference.validate()?;
        Ok(reference)
    }

    fn pixels(&self) -> u64 {
        u64::from(self.image.width) * u64::from(self.image.height)
    }

    fn validate(&self) -> Result<()> {
        let suffix = self
            .id
            .strip_prefix("file_")
            .ok_or("invalid managed file ID")?;
        if self.marker != "file" || self.version != 1 {
            return Err("unsupported managed File reference version".into());
        }
        if suffix.len() != 64
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !valid_name(&self.name)
            || !matches!(self.mime_type.as_str(), "image/png" | "image/jpeg")
            || self.size_bytes == 0
            || self.size_bytes > MAX_FILE_BYTES
            || self.image.width == 0
            || self.image.height == 0
            || self.image.width > MAX_DIMENSION
            || self.image.height > MAX_DIMENSION
            || self.pixels() > MAX_PIXELS
        {
            return Err("invalid managed File reference metadata".into());
        }
        Ok(())
    }
}

#[derive(Default)]
struct Selection {
    nodes: usize,
    files: Vec<(String, FileReference)>,
}

impl Selection {
    fn walk(&mut self, value: &Value, pointer: String, depth: usize) -> Result<()> {
        self.nodes += 1;
        if self.nodes > MAX_NODES || depth > MAX_DEPTH || pointer.len() > 2048 {
            return Err("compose return exceeds file-selection traversal budget".into());
        }
        match value {
            Value::Object(object) if object.get("$kit").and_then(Value::as_str) == Some("file") => {
                if self.files.len() >= MAX_OCCURRENCES {
                    return Err("compose return exceeds 64 File reference occurrences".into());
                }
                self.files
                    .push((pointer, FileReference::from_value(value)?));
            }
            Value::Object(object) => {
                // Check before allocating a sorted traversal frontier.
                if object.len() > MAX_NODES - self.nodes {
                    return Err("compose return exceeds file-selection traversal budget".into());
                }
                let mut entries = object.iter().collect::<Vec<_>>();
                entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
                for (key, child) in entries {
                    if key.len() > MAX_HEADER_BYTES {
                        return Err("compose return key exceeds file-selection label budget".into());
                    }
                    self.walk(
                        child,
                        format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                        depth + 1,
                    )?;
                }
            }
            Value::Array(array) => {
                if array.len() > MAX_NODES - self.nodes {
                    return Err("compose return exceeds file-selection traversal budget".into());
                }
                for (index, child) in array.iter().enumerate() {
                    self.walk(child, format!("{pointer}/{index}"), depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Native exclusive rename for both files and directories. Staging and target
/// are always in the same managed store. Never fall back to check-then-rename
/// or hard links: those break no-clobber or secure single-link resolution.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both pointers reference live NUL-terminated path strings. These
    // calls do not retain pointers. Flags request an atomic no-replace rename.
    let result = unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        }
        #[cfg(target_os = "macos")]
        {
            libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL)
        }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().collect();
    if source.contains(&0) || destination.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    let source = [source, vec![0]].concat();
    let destination = [destination, vec![0]].concat();
    // SAFETY: live NUL-terminated UTF-16 paths; the call retains no pointers.
    // Unlike std::fs::rename, flags 0 excludes MOVEFILE_REPLACE_EXISTING.
    let result = unsafe { move_file_ex(source.as_ptr(), destination.as_ptr(), 0) };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic exclusive managed-file rename is unavailable on this platform",
    ))
}

/// Validate native provider bytes without publishing them or granting authority.
pub(crate) fn validate_provider_image(bytes: &[u8]) -> Result<(String, u64)> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("image must contain 1 byte to 8 MiB".into());
    }
    let (mime, dimensions) = inspect_image(bytes)?;
    Ok((
        mime,
        u64::from(dimensions.width) * u64::from(dimensions.height),
    ))
}

fn inspect_image(bytes: &[u8]) -> Result<(String, ImageDimensions)> {
    let format = image::guess_format(bytes).map_err(display)?;
    let decoder = bounded_decoder(bytes, format)?;
    let (width, height) = decoder.dimensions();
    // Validate pixels without changing the original bytes or dimensions.
    DynamicImage::from_decoder(decoder).map_err(display)?;
    Ok((
        format.to_mime_type().into(),
        ImageDimensions { width, height },
    ))
}

fn bounded_decoder(bytes: &[u8], format: ImageFormat) -> Result<Box<dyn ImageDecoder + '_>> {
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err("read_file supports only nonanimated PNG and JPEG images".into());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    let decoder: Box<dyn ImageDecoder> = if format == ImageFormat::Png {
        operations::check_png_metadata(bytes)?;
        let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits)
            .map_err(display)?;
        if decoder.is_apng().map_err(display)? {
            return Err("animated PNG images are not supported by read_file".into());
        }
        Box::new(decoder)
    } else {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits);
        Box::new(reader.into_decoder().map_err(display)?)
    };
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODE_BYTES
    {
        return Err("image exceeds decoded pixel or allocation budget".into());
    }
    Ok(decoder)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
}

fn check_cancelled(cancellation: Option<&TurnCancellation>) -> Result<()> {
    if cancellation.is_some_and(TurnCancellation::is_cancelled) {
        Err("managed file operation cancelled".into())
    } else {
        Ok(())
    }
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests;
