//! Native filesystem boundary. Ownership never falls back to an in-process lock.
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Stable native file identity, scoped to a volume. Not a content fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume: u64,
    pub file: u64,
}
#[cfg(unix)]
fn metadata_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    }
}

#[derive(Default, Clone, Debug)]
pub struct DiskOpenOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
    pub private: bool,
    /// Native open flags. Access, creation and truncation use the fields above.
    pub custom_flags: i32,
}
#[derive(Clone, Debug)]
pub struct DiskEntry {
    pub path: PathBuf,
    pub file_name: OsString,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseMode {
    CreateNew,
    ExistingOrNew,
}
#[derive(Clone, Debug)]
pub struct LeaseRequest {
    pub path: PathBuf,
    pub scope: PathBuf,
    pub mode: LeaseMode,
    pub remove_on_drop: bool,
}
pub trait Backend: Send + Sync {
    fn open(&self, path: &Path, options: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>>;
    fn metadata(&self, path: &Path, follow: bool) -> io::Result<fs::Metadata>;
    fn identity(&self, path: &Path, follow: bool) -> io::Result<Option<FileIdentity>> {
        #[cfg(unix)]
        {
            self.metadata(path, follow)
                .map(|m| Some(metadata_identity(&m)))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, follow);
            Ok(None)
        }
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DiskEntry>>;
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn create_dir(&self, path: &Path, private: bool) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn set_permissions(&self, path: &Path, p: Permissions) -> io::Result<()>;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
    fn acquire_lease(&self, request: &LeaseRequest) -> io::Result<Box<dyn BackendLease>>;
    fn open_beneath(&self, root: &Path, relative: &Path) -> io::Result<Box<dyn BackendFile>>;
}
pub trait BackendFile: Read + Write + Seek + Send {
    fn metadata(&self) -> io::Result<fs::Metadata>;
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        #[cfg(unix)]
        {
            self.metadata().map(|m| Some(metadata_identity(&m)))
        }
        #[cfg(not(unix))]
        {
            Ok(None)
        }
    }
    fn set_len(&self, size: u64) -> io::Result<()>;
    fn sync_data(&self) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
    fn set_permissions(&self, p: Permissions) -> io::Result<()>;
}
pub trait BackendLease: Send + Sync {
    fn check(&self) -> io::Result<()>;
}
impl BackendFile for File {
    fn metadata(&self) -> io::Result<fs::Metadata> {
        File::metadata(self)
    }
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        native::file_identity(self).map(Some)
    }
    fn set_len(&self, size: u64) -> io::Result<()> {
        File::set_len(self, size)
    }
    fn sync_data(&self) -> io::Result<()> {
        File::sync_data(self)
    }
    fn sync_all(&self) -> io::Result<()> {
        File::sync_all(self)
    }
    fn set_permissions(&self, p: Permissions) -> io::Result<()> {
        File::set_permissions(self, p)
    }
}
#[derive(Default)]
pub struct DiskBackend;
impl Backend for DiskBackend {
    fn open(&self, path: &Path, o: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        Ok(Box::new(native::open(path, o)?))
    }
    fn metadata(&self, path: &Path, follow: bool) -> io::Result<fs::Metadata> {
        if follow {
            fs::metadata(path)
        } else {
            fs::symlink_metadata(path)
        }
    }
    fn identity(&self, path: &Path, follow: bool) -> io::Result<Option<FileIdentity>> {
        native::path_identity(path, follow).map(Some)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DiskEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            entries.try_reserve(1).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "directory inventory allocation failed",
                )
            })?;
            entries.push(DiskEntry {
                path: entry.path(),
                file_name: entry.file_name(),
            });
        }
        Ok(entries)
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        fs::read_link(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
    fn create_dir(&self, path: &Path, private: bool) -> io::Result<()> {
        native::create_dir(path, private)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        native::remove(path, false)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        native::remove(path, true)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        native::rename(from, to)
    }
    fn set_permissions(&self, path: &Path, p: Permissions) -> io::Result<()> {
        native::set_permissions(path, p)
    }
    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        native::sync_directory(path)
    }
    fn acquire_lease(&self, request: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
        acquire_lease(request)
    }
    fn open_beneath(&self, root: &Path, relative: &Path) -> io::Result<Box<dyn BackendFile>> {
        Ok(Box::new(native::open_beneath(root, relative)?))
    }
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "file identity, token, or private ownership changed",
    )
}
#[cfg(unix)]
mod native {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
    use std::path::Component;
    fn denied() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lease identity, token, or private ownership changed",
        )
    }
    fn open_at(parent: &File, name: &std::ffi::OsStr, directory: bool) -> io::Result<File> {
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent is live, name is NUL terminated, and no creation mode is needed.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh descriptor owned by this function.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub(super) fn open_beneath(root: &Path, relative: &Path) -> io::Result<File> {
        let parts: Vec<_> = relative.components().collect();
        if parts.is_empty() || parts.iter().any(|p| !matches!(p, Component::Normal(_))) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected nonempty relative path without traversal",
            ));
        }
        let mut dir = parent(&root.join(".kit-root-anchor"))?.dir;
        for (index, part) in parts.iter().enumerate() {
            if let Component::Normal(name) = part {
                dir = open_at(&dir, name, index + 1 < parts.len())?;
            }
        }
        if !dir.metadata()?.is_file() || dir.metadata()?.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a regular file",
            ));
        }
        Ok(dir)
    }
    pub(super) struct Parent {
        dir: File,
        name: CString,
        path: PathBuf,
    }
    pub(super) fn parent(path: &Path) -> io::Result<Parent> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open("/")?;
        let mut parts = absolute.components().peekable();
        while let Some(part) = parts.next() {
            match part {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) if parts.peek().is_none() => {
                    return Ok(Parent {
                        path: absolute.clone(),
                        dir,
                        name: CString::new(name.as_bytes()).map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL")
                        })?,
                    });
                }
                Component::Normal(name) => dir = open_at(&dir, name, true)?,
                _ => return Err(denied()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a file name",
        ))
    }
    impl Parent {
        pub(super) fn open(&self, o: &DiskOpenOptions) -> io::Result<File> {
            if !(o.read || o.write || o.append)
                || ((o.truncate || o.create || o.create_new) && !(o.write || o.append))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid open options",
                ));
            }
            let access = if o.read && (o.write || o.append) {
                libc::O_RDWR
            } else if o.write || o.append {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            };
            // Never truncate before checking the opened inode. In particular a
            // replay image must not change another file through a hard link.
            let flags = access
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | (o.custom_flags
                    & !(libc::O_ACCMODE | libc::O_TRUNC | libc::O_CREAT | libc::O_EXCL))
                | if o.append { libc::O_APPEND } else { 0 }
                | if o.create_new {
                    libc::O_CREAT | libc::O_EXCL
                } else if o.create {
                    libc::O_CREAT
                } else {
                    0
                };
            // SAFETY: live directory descriptor and NUL-terminated name; mode supplied for creation.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    self.name.as_ptr(),
                    flags,
                    if o.private { 0o600 } else { 0o666 },
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd is newly owned.
            let file = unsafe { File::from_raw_fd(fd) };
            let held = file.metadata()?;
            if !held.is_file() || held.nlink() != 1 {
                return Err(denied());
            }
            self.check(&file)?;
            if o.truncate {
                file.set_len(0)?;
            }
            Ok(file)
        }
        pub(super) fn check(&self, file: &File) -> io::Result<()> {
            let current = parent(&self.path)?;
            let named = open_at(
                &current.dir,
                std::ffi::OsStr::from_bytes(current.name.as_bytes()),
                false,
            )?;
            let held_dir = self.dir.metadata()?;
            let named_dir = current.dir.metadata()?;
            if held_dir.dev() != named_dir.dev() || held_dir.ino() != named_dir.ino() {
                return Err(denied());
            }
            let a = file.metadata()?;
            let b = named.metadata()?;
            if !a.is_file() || a.nlink() != 1 || a.dev() != b.dev() || a.ino() != b.ino() {
                return Err(denied());
            }
            Ok(())
        }
        pub(super) fn remove(&self, directory: bool) -> io::Result<()> {
            // SAFETY: descriptors and names remain live through the syscall.
            let result = unsafe {
                libc::unlinkat(
                    self.dir.as_raw_fd(),
                    self.name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
    pub(super) fn open(path: &Path, o: &DiskOpenOptions) -> io::Result<File> {
        parent(path)?.open(o)
    }
    pub(super) fn remove(path: &Path, directory: bool) -> io::Result<()> {
        parent(path)?.remove(directory)
    }
    pub(super) fn create_dir(path: &Path, private: bool) -> io::Result<()> {
        let p = parent(path)?;
        // SAFETY: live descriptor, terminated name and valid mode.
        if unsafe {
            libc::mkdirat(
                p.dir.as_raw_fd(),
                p.name.as_ptr(),
                if private { 0o700 } else { 0o777 },
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        let a = parent(from)?;
        let b = parent(to)?;
        // SAFETY: both parent descriptors and names remain live.
        if unsafe {
            libc::renameat(
                a.dir.as_raw_fd(),
                a.name.as_ptr(),
                b.dir.as_raw_fd(),
                b.name.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn set_permissions(path: &Path, permissions: Permissions) -> io::Result<()> {
        let p = parent(path)?;
        let file = open_at(
            &p.dir,
            std::ffi::OsStr::from_bytes(p.name.as_bytes()),
            false,
        )?;
        let m = file.metadata()?;
        if !m.is_dir() && (!m.is_file() || m.nlink() != 1) {
            return Err(denied());
        }
        file.set_permissions(permissions)
    }
    pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
        let p = parent(&path.join(".kit-sync-anchor"))?;
        p.dir.sync_all()
    }
    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        file.metadata().map(|m| metadata_identity(&m))
    }
    pub(super) fn path_identity(path: &Path, follow: bool) -> io::Result<FileIdentity> {
        if path.file_name().is_none() {
            return file_identity(&parent(&path.join(".kit-identity-anchor"))?.dir);
        }
        let p = parent(path)?;
        // fstatat reads identity relative to the pinned parent, including a
        // symlink itself when follow=false. It never opens for mutation.
        let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: live parent descriptor, terminated name, writable stat storage.
        if unsafe {
            libc::fstatat(
                p.dir.as_raw_fd(),
                p.name.as_ptr(),
                info.as_mut_ptr(),
                if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW },
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized the structure.
        let info = unsafe { info.assume_init() };
        // dev_t is signed on macOS and u64 on Linux.
        #[allow(clippy::unnecessary_cast)]
        Ok(FileIdentity {
            volume: info.st_dev as u64,
            file: info.st_ino,
        })
    }
    pub(super) fn read_token(file: &File, token: &mut [u8]) -> io::Result<()> {
        file.read_exact_at(token, 0)
    }
    pub(super) fn owned_regular(metadata: &fs::Metadata) -> bool {
        // Lock tokens are not credential data. Historical session locks can
        // have mode 0644, but must still be a single-link file owned by us.
        // SAFETY: geteuid has no preconditions.
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1
    }
    pub(super) fn private_regular(metadata: &fs::Metadata) -> bool {
        owned_regular(metadata) && metadata.mode() & 0o077 == 0
    }
    pub(super) fn tighten_owned_lease(file: &File) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file.metadata()?;
        if !owned_regular(&metadata) {
            return Err(denied());
        }
        if metadata.mode() & 0o077 != 0 {
            // Called only after real OS exclusion and named identity checks.
            // Change the held lock inode, never a pathname or unrelated data.
            file.set_permissions(Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::Component;
    const REPARSE: u32 = 0x00200000;
    const BACKUP: u32 = 0x02000000;
    const REPARSE_ATTRIBUTE: u32 = 0x400;
    #[repr(C)]
    #[derive(Default)]
    struct FileInfo {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut FileInfo) -> i32;
    }
    fn file_info(file: &File) -> io::Result<FileInfo> {
        let mut info = FileInfo::default();
        // SAFETY: live file handle and correctly sized writable Win32 structure.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    }
    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        let info = file_info(file)?;
        Ok(FileIdentity {
            volume: u64::from(info.volume),
            file: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        })
    }
    pub(super) fn path_identity(path: &Path, follow: bool) -> io::Result<FileIdentity> {
        // Keep the same ancestor reparse protection as mutation opens, but
        // permit directories and query without write access or truncation.
        let p = if path.file_name().is_none() {
            parent(&path.join(".kit-identity-anchor"))?
        } else {
            parent(path)?
        };
        let named = if path.file_name().is_none() {
            p.path.parent().ok_or_else(denied)?
        } else {
            &p.path
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(BACKUP | if follow { 0 } else { REPARSE })
            .open(named)?;
        file_identity(&file)
    }
    fn identity(file: &File) -> io::Result<FileIdentity> {
        let info = file_info(file)?;
        if info.links != 1
            || info.attributes & REPARSE_ATTRIBUTE != 0
            || !file.metadata()?.is_file()
        {
            return Err(denied());
        }
        Ok(FileIdentity {
            volume: u64::from(info.volume),
            file: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        })
    }
    fn directory(path: &Path) -> io::Result<File> {
        // Denying write/delete sharing pins every ancestor against rename and
        // reparse-point modification until the operation has completed.
        let f = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(REPARSE | BACKUP)
            .open(path)?;
        let m = f.metadata()?;
        if !m.is_dir() || m.file_attributes() & REPARSE_ATTRIBUTE != 0 {
            return Err(denied());
        }
        Ok(f)
    }
    pub(super) struct Parent {
        path: PathBuf,
        _ancestors: Vec<File>,
    }
    pub(super) fn parent(path: &Path) -> io::Result<Parent> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if absolute.file_name().is_none() {
            return Err(denied());
        }
        let mut prefix = PathBuf::new();
        let mut ancestors = Vec::new();
        for part in absolute.parent().ok_or_else(denied)?.components() {
            match part {
                Component::Prefix(_) => prefix.push(part.as_os_str()),
                Component::RootDir | Component::Normal(_) => {
                    prefix.push(part.as_os_str());
                    ancestors.push(directory(&prefix)?);
                }
                Component::CurDir => {}
                _ => return Err(denied()),
            }
        }
        Ok(Parent {
            path: absolute,
            _ancestors: ancestors,
        })
    }
    impl Parent {
        pub(super) fn open(&self, o: &DiskOpenOptions) -> io::Result<File> {
            let f = OpenOptions::new()
                .read(o.read)
                .write(o.write)
                .append(o.append)
                .create(o.create)
                .create_new(o.create_new)
                .custom_flags(REPARSE | o.custom_flags as u32)
                .open(&self.path)?;
            identity(&f)?;
            self.check(&f)?;
            if o.truncate {
                f.set_len(0)?;
            }
            // Windows private files/directories inherit the containing user's ACL,
            // as the pre-resilient backend did; POSIX mode bits do not model ACLs.
            Ok(f)
        }
        pub(super) fn check(&self, file: &File) -> io::Result<()> {
            let named = OpenOptions::new()
                .read(true)
                .custom_flags(REPARSE)
                .open(&self.path)?;
            if identity(file)? != identity(&named)? {
                return Err(denied());
            }
            Ok(())
        }
        pub(super) fn remove(&self, directory: bool) -> io::Result<()> {
            if directory {
                fs::remove_dir(&self.path)
            } else {
                fs::remove_file(&self.path)
            }
        }
    }
    pub(super) fn open(path: &Path, o: &DiskOpenOptions) -> io::Result<File> {
        parent(path)?.open(o)
    }
    pub(super) fn remove(path: &Path, directory: bool) -> io::Result<()> {
        parent(path)?.remove(directory)
    }
    pub(super) fn create_dir(path: &Path, _private: bool) -> io::Result<()> {
        let p = parent(path)?;
        fs::create_dir(&p.path)
    }
    pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        let a = parent(from)?;
        let b = parent(to)?;
        fs::rename(&a.path, &b.path)
    }
    pub(super) fn set_permissions(path: &Path, permissions: Permissions) -> io::Result<()> {
        let p = parent(path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(REPARSE | BACKUP)
            .open(&p.path)?;
        let m = file.metadata()?;
        if m.file_attributes() & REPARSE_ATTRIBUTE != 0 {
            return Err(denied());
        }
        if !m.is_dir() {
            identity(&file)?;
        }
        file.set_permissions(permissions)
    }
    pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
        let _p = parent(&path.join(".kit-sync-anchor"))?;
        // Windows does not support FlushFileBuffers on directory handles.
        Ok(())
    }
    pub(super) fn open_beneath(root: &Path, relative: &Path) -> io::Result<File> {
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|p| !matches!(p, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected nonempty relative path without traversal",
            ));
        }
        open(
            &root.join(relative),
            &DiskOpenOptions {
                read: true,
                ..Default::default()
            },
        )
    }
    pub(super) fn owned_regular(m: &fs::Metadata) -> bool {
        private_regular(m)
    }
    pub(super) fn tighten_owned_lease(file: &File) -> io::Result<()> {
        // Windows retains its inherited ACL; POSIX modes do not apply.
        identity(file).map(|_| ())
    }
    pub(super) fn private_regular(m: &fs::Metadata) -> bool {
        m.is_file() && m.file_attributes() & REPARSE_ATTRIBUTE == 0
    }
    pub(super) fn read_token(file: &File, mut token: &mut [u8]) -> io::Result<()> {
        let mut offset = 0;
        while !token.is_empty() {
            let n = file.seek_read(token, offset)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            offset += n as u64;
            token = &mut token[n..];
        }
        Ok(())
    }
}

struct DiskLease {
    file: File,
    parent: native::Parent,
    token: [u8; 64],
    remove_on_drop: bool,
}
impl DiskLease {
    fn check_identity(&self) -> io::Result<()> {
        self.parent.check(&self.file)?;
        if !native::private_regular(&self.file.metadata()?) {
            return Err(denied());
        }
        Ok(())
    }
}
impl BackendLease for DiskLease {
    fn check(&self) -> io::Result<()> {
        self.check_identity()?;
        if self.file.metadata()?.len() != self.token.len() as u64 {
            return Err(denied());
        }
        let mut token = [0; 64];
        native::read_token(&self.file, &mut token)?;
        if token != self.token {
            return Err(denied());
        }
        self.check_identity()
    }
}
impl Drop for DiskLease {
    fn drop(&mut self) {
        // OS ownership remains held through cleanup. The final unlink assumes
        // other actors with write access to this private directory cooperate.
        if self.remove_on_drop && self.check().is_ok() {
            let _ = self.parent.remove(false);
        }
    }
}
fn initialize_lease(
    lease: &mut DiskLease,
    created: bool,
    initialize: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let result = lease
        .check_identity()
        .and_then(|()| initialize(&mut lease.file, &lease.token))
        .and_then(|()| lease.check());
    if result.is_err() && created && lease.check_identity().is_ok() {
        // Initialization may have failed before a complete token was written.
        // Identity, not token equality, authorizes rollback of our new inode.
        // The OS lock is still held, and a losing acquirer never reaches here.
        lease.parent.remove(false)?;
    }
    result
}
fn acquire_lease(request: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
    let mut random = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *random).map_err(io::Error::other)?;
    let mut token = [0; 64];
    for (i, byte) in random.iter().enumerate() {
        token[i * 2] = b"0123456789abcdef"[(byte >> 4) as usize];
        token[i * 2 + 1] = b"0123456789abcdef"[(byte & 15) as usize];
    }
    let parent = native::parent(&request.path)?;
    let mut options = DiskOpenOptions {
        read: true,
        write: true,
        create_new: true,
        private: true,
        ..Default::default()
    };
    let (file, created) = match parent.open(&options) {
        Ok(file) => (file, true),
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists
                && request.mode == LeaseMode::ExistingOrNew =>
        {
            options.create_new = false;
            (parent.open(&options)?, false)
        }
        Err(e) => return Err(e),
    };
    if !native::owned_regular(&file.metadata()?) {
        return Err(denied());
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "lease is held by another owner",
            ));
        }
        Err(fs::TryLockError::Error(e)) => return Err(e),
    }
    // Never tighten a live historical owner's permissions. Only the winner
    // can validate and secure this inode before replacing its opaque token.
    if !created {
        parent.check(&file)?;
        native::tighten_owned_lease(&file)?;
    }
    let mut lease = DiskLease {
        file,
        parent,
        token,
        remove_on_drop: false,
    };
    initialize_lease(&mut lease, created, |file, token| {
        file.set_len(0)?;
        file.write_all(token)?;
        file.sync_all()
    })?;
    lease.remove_on_drop = request.remove_on_drop;
    Ok(Box::new(lease))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kit-native-fs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn request(&self, remove_on_drop: bool) -> LeaseRequest {
            LeaseRequest {
                path: self.0.join("lock"),
                scope: self.0.clone(),
                mode: LeaseMode::ExistingOrNew,
                remove_on_drop,
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn stable_identity_matches_handles_directories_and_symlink_semantics() {
        let temp = Temp::new();
        let path = temp.0.join("data");
        fs::write(&path, b"data").unwrap();
        let file = DiskBackend
            .open(
                &path,
                &DiskOpenOptions {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let id = file.identity().unwrap().unwrap();
        assert_eq!(Some(id), DiskBackend.identity(&path, false).unwrap());
        assert_eq!(
            Some(id),
            BackendFile::identity(&File::open(&path).unwrap()).unwrap()
        );
        let directory = File::open(&temp.0).unwrap();
        assert_eq!(
            BackendFile::identity(&directory).unwrap(),
            DiskBackend.identity(&temp.0, false).unwrap()
        );
        assert!(
            DiskBackend
                .identity(Path::new("/"), false)
                .unwrap()
                .is_some()
        );
        let link = temp.0.join("link");
        symlink(&path, &link).unwrap();
        assert_eq!(Some(id), DiskBackend.identity(&link, true).unwrap());
        assert_ne!(Some(id), DiskBackend.identity(&link, false).unwrap());
        fs::rename(&path, temp.0.join("old")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_ne!(Some(id), DiskBackend.identity(&path, false).unwrap());
        assert_eq!(Some(id), file.identity().unwrap());
    }
    #[test]
    fn absent_lease_paths_report_not_found_but_replacements_remain_fenced() {
        let temp = Temp::new();
        let dir = temp.0.join("dir");
        fs::create_dir(&dir).unwrap();
        let request = LeaseRequest {
            path: dir.join("lock"),
            scope: dir.clone(),
            mode: LeaseMode::CreateNew,
            remove_on_drop: true,
        };
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        fs::remove_file(&request.path).unwrap();
        assert_eq!(lease.check().unwrap_err().kind(), io::ErrorKind::NotFound);
        drop(lease);
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        fs::rename(&dir, temp.0.join("old")).unwrap();
        assert_eq!(lease.check().unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::create_dir(&dir).unwrap();
        assert_eq!(lease.check().unwrap_err().kind(), io::ErrorKind::NotFound);
        let replacement = DiskBackend.acquire_lease(&request).unwrap();
        assert_eq!(
            lease.check().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(lease);
        replacement.check().unwrap();
    }
    #[test]
    fn historical_ascii_lock_takeover_stays_utf8() {
        let temp = Temp::new();
        let request = temp.request(false);
        fs::write(&request.path, "historical-owner-123\n").unwrap();
        fs::set_permissions(&request.path, Permissions::from_mode(0o644)).unwrap();
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        let token = fs::read_to_string(&request.path).unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        lease.check().unwrap();
        assert_eq!(fs::metadata(&request.path).unwrap().mode() & 0o777, 0o600);
    }
    #[test]
    fn live_historical_ascii_lock_preserves_permissions_and_token() {
        let temp = Temp::new();
        let request = temp.request(true);
        let mut owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&request.path)
            .unwrap();
        owner
            .set_permissions(Permissions::from_mode(0o644))
            .unwrap();
        owner.try_lock().unwrap();
        owner.write_all(b"historical-live-owner\n").unwrap();
        owner.sync_all().unwrap();
        assert_eq!(
            DiskBackend.acquire_lease(&request).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(fs::metadata(&request.path).unwrap().mode() & 0o777, 0o644);
        assert_eq!(
            fs::read_to_string(&request.path).unwrap(),
            "historical-live-owner\n"
        );
        assert_eq!(
            fs::metadata(&request.path).unwrap().ino(),
            owner.metadata().unwrap().ino()
        );
        drop(owner);
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        lease.check().unwrap();
        assert_eq!(fs::metadata(&request.path).unwrap().mode() & 0o777, 0o600);
    }
    fn new_test_lease(path: &Path) -> DiskLease {
        let parent = native::parent(path).unwrap();
        let file = parent
            .open(&DiskOpenOptions {
                read: true,
                write: true,
                create_new: true,
                private: true,
                ..Default::default()
            })
            .unwrap();
        file.try_lock().unwrap();
        DiskLease {
            file,
            parent,
            token: [b'a'; 64],
            remove_on_drop: false,
        }
    }
    #[test]
    fn initialization_write_and_sync_errors_remove_own_new_inode() {
        for after_write in [false, true] {
            let temp = Temp::new();
            let path = temp.0.join("lock");
            let mut lease = new_test_lease(&path);
            let result = initialize_lease(&mut lease, true, |file, token| {
                if after_write {
                    file.write_all(token)?;
                }
                // Exercise the actual initialization rollback with a failing IO
                // operation, without adding fault controls to production state.
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            });
            assert!(result.is_err());
            assert!(!path.exists());
            assert_eq!(lease.file.metadata().unwrap().nlink(), 0);
        }
    }
    #[test]
    fn initialization_failure_never_removes_replacement_or_existing_inode() {
        let temp = Temp::new();
        let path = temp.0.join("lock");
        let mut lease = new_test_lease(&path);
        assert!(
            initialize_lease(&mut lease, false, |_, _| Err(
                io::ErrorKind::StorageFull.into()
            ))
            .is_err()
        );
        assert!(path.exists());
        assert!(
            initialize_lease(&mut lease, true, |_, _| {
                fs::rename(&path, temp.0.join("old"))?;
                fs::write(&path, b"replacement")?;
                Err(io::ErrorKind::StorageFull.into())
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }
    #[test]
    fn mutation_parents_and_hardlink_truncation_are_rejected() {
        let temp = Temp::new();
        let real = temp.0.join("real");
        fs::create_dir(&real).unwrap();
        symlink(&real, temp.0.join("alias")).unwrap();
        assert!(
            DiskBackend
                .create_dir(&temp.0.join("alias/child"), true)
                .is_err()
        );
        let file = real.join("file");
        fs::write(&file, b"unchanged").unwrap();
        fs::hard_link(&file, real.join("link")).unwrap();
        assert!(
            DiskBackend
                .open(
                    &file,
                    &DiskOpenOptions {
                        write: true,
                        truncate: true,
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert_eq!(fs::read(&file).unwrap(), b"unchanged");
        assert!(DiskBackend.remove_file(&temp.0.join("alias/file")).is_err());
        assert!(
            DiskBackend
                .rename(&file, &temp.0.join("alias/new"))
                .is_err()
        );
    }
    #[test]
    fn lease_parent_replacement_fences_owner() {
        let temp = Temp::new();
        let dir = temp.0.join("dir");
        fs::create_dir(&dir).unwrap();
        let request = LeaseRequest {
            path: dir.join("lock"),
            scope: dir.clone(),
            mode: LeaseMode::CreateNew,
            remove_on_drop: true,
        };
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        fs::rename(&dir, temp.0.join("old")).unwrap();
        fs::create_dir(&dir).unwrap();
        assert!(lease.check().is_err());
        drop(lease);
        assert!(temp.0.join("old/lock").exists());
    }
    #[test]
    fn private_creation_and_io() {
        let temp = Temp::new();
        let dir = temp.0.join("private");
        DiskBackend.create_dir(&dir, true).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = dir.join("data");
        let mut file = DiskBackend
            .open(
                &path,
                &DiskOpenOptions {
                    read: true,
                    write: true,
                    create_new: true,
                    private: true,
                    ..Default::default()
                },
            )
            .unwrap();
        file.write_all(b"contents").unwrap();
        file.sync_all().unwrap();
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"contents");
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(DiskBackend.read_dir(&dir).unwrap()[0].path, path);
        DiskBackend.sync_directory(&dir).unwrap();
    }
    #[test]
    fn secure_traversal_rejects_symlinks_and_escape() {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("dir")).unwrap();
        fs::write(temp.0.join("dir/file"), b"safe").unwrap();
        symlink("dir", temp.0.join("alias")).unwrap();
        symlink("dir/file", temp.0.join("link")).unwrap();
        assert!(
            DiskBackend
                .open_beneath(&temp.0, Path::new("dir/file"))
                .is_ok()
        );
        for name in ["alias/file", "link", "../outside", "/etc/passwd", "", "dir"] {
            assert!(
                DiskBackend.open_beneath(&temp.0, Path::new(name)).is_err(),
                "{name}"
            );
        }
    }
    #[test]
    fn real_lock_exclusion_and_retention() {
        let temp = Temp::new();
        let request = temp.request(false);
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        lease.check().unwrap();
        let error = DiskBackend.acquire_lease(&request).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        lease.check().unwrap(); // Failed contender must not change the token.
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!(
                    "{}::lease_child_probe",
                    module_path!()
                        .split_once("::")
                        .map_or(module_path!(), |(_, path)| path)
                ),
                "--quiet",
            ])
            .env("KIT_NATIVE_LEASE_TEST_PATH", &request.path)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stdout)
        );
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("running 1 test"),
            "child filter did not run the lease probe: {}",
            String::from_utf8_lossy(&child.stdout)
        );
        drop(lease);
        assert!(request.path.exists());
        let next = DiskBackend.acquire_lease(&request).unwrap();
        next.check().unwrap();
    }
    #[test]
    fn lease_child_probe() {
        let Some(path) = std::env::var_os("KIT_NATIVE_LEASE_TEST_PATH") else {
            return;
        };
        let request = LeaseRequest {
            path: PathBuf::from(path),
            scope: PathBuf::new(),
            mode: LeaseMode::ExistingOrNew,
            remove_on_drop: false,
        };
        assert_eq!(
            DiskBackend.acquire_lease(&request).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn cleanup_and_create_new() {
        let temp = Temp::new();
        let mut request = temp.request(true);
        request.mode = LeaseMode::CreateNew;
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        assert_eq!(
            DiskBackend.acquire_lease(&request).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(lease);
        assert!(!request.path.exists());
    }
    #[test]
    fn token_and_replacement_fence_old_owner() {
        let temp = Temp::new();
        let request = temp.request(true);
        let lease = DiskBackend.acquire_lease(&request).unwrap();
        fs::write(&request.path, [0; 32]).unwrap();
        assert!(lease.check().is_err());
        drop(lease);
        assert!(request.path.exists());
        // Another concurrently running test can fork while this descriptor is
        // live. Its CLOEXEC duplicate releases at exec, not at our local drop.
        let mut attempts = 0;
        let lease = loop {
            match DiskBackend.acquire_lease(&request) {
                Ok(lease) => break lease,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && attempts < 100 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("lease remained held after drop: {e}"),
            }
        };
        fs::rename(&request.path, temp.0.join("old")).unwrap();
        let replacement = DiskBackend.acquire_lease(&request).unwrap();
        assert!(lease.check().is_err());
        drop(lease);
        replacement.check().unwrap();
        drop(replacement);
        assert!(!request.path.exists());
    }
    #[test]
    fn unsafe_lock_files_rejected_without_modification() {
        let temp = Temp::new();
        let request = temp.request(false);
        let target = temp.0.join("target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, &request.path).unwrap();
        assert!(DiskBackend.acquire_lease(&request).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_file(&request.path).unwrap();
        fs::write(&request.path, b"public").unwrap();
        fs::set_permissions(&request.path, Permissions::from_mode(0o644)).unwrap();
        fs::hard_link(&request.path, temp.0.join("hardlink")).unwrap();
        assert!(DiskBackend.acquire_lease(&request).is_err());
        assert_eq!(fs::read(&request.path).unwrap(), b"public");
        assert_eq!(fs::metadata(&request.path).unwrap().mode() & 0o777, 0o644);
    }
}
