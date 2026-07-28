use std::{
    ffi::CStr,
    fs::File,
    io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, FromRawFd},
};

pub(super) const MODE_DIRECTORY: u32 = libc::S_IFDIR as u32;
pub(super) const MODE_REGULAR: u32 = libc::S_IFREG as u32;
pub(super) const MODE_SYMLINK: u32 = libc::S_IFLNK as u32;

pub(super) fn open_file_component(directory: &File, name: &CStr) -> io::Result<File> {
    open_component(directory, name, libc::O_RDONLY | libc::O_NONBLOCK)
}

pub(super) fn open_directory_component(directory: &File, name: &CStr) -> io::Result<File> {
    open_component(directory, name, libc::O_RDONLY | libc::O_DIRECTORY)
}

pub(super) fn open_filesystem_root() -> io::Result<File> {
    let descriptor = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(target_os = "linux")]
pub(super) fn ensure_local_filesystem(file: &File) -> io::Result<()> {
    let mut metadata = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let filesystem = unsafe { metadata.assume_init() }.f_type as u64;
    if matches!(
        filesystem,
        0x0000_EF53
            | 0x0102_1994
            | 0x2FC1_2FC1
            | 0x5846_5342
            | 0x794C_7630
            | 0x8584_58F6
            | 0x9123_683E
            | 0xF2F5_2010
    ) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported nonlocal filesystem",
        ))
    }
}

#[cfg(target_os = "macos")]
pub(super) fn ensure_local_filesystem(file: &File) -> io::Result<()> {
    let mut metadata = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { metadata.assume_init() }.f_flags & libc::MNT_LOCAL as u32 != 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported nonlocal filesystem",
        ))
    }
}

// This is the sole path_auth entry point for opening an untrusted component.
// Linux uses kernel-enforced beneath resolution; the fallback opens one no-follow leaf.
fn open_component(directory: &File, name: &CStr, requested_flags: libc::c_int) -> io::Result<File> {
    let flags = requested_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    {
        let mut how = unsafe { MaybeUninit::<libc::open_how>::zeroed().assume_init() };
        how.flags = flags as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
        let descriptor = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                directory.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of::<libc::open_how>(),
            ) as libc::c_int
        };
        if descriptor >= 0 {
            return Ok(unsafe { File::from_raw_fd(descriptor) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOSYS) {
            return Err(error);
        }
    }
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

pub(super) fn stat_at(directory: &File, name: &CStr) -> io::Result<Stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::from_raw(unsafe { stat.assume_init() }))
}

pub(super) fn stat_file(file: &File) -> io::Result<Stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::from_raw(unsafe { stat.assume_init() }))
}

pub(super) struct DirectoryStream {
    stream: *mut libc::DIR,
}

impl DirectoryStream {
    pub(super) fn open(directory: &File) -> io::Result<Self> {
        let descriptor = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            unsafe { libc::close(descriptor) };
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::rewinddir(stream) };
        Ok(Self { stream })
    }

    pub(super) fn next(&mut self) -> io::Result<Option<&CStr>> {
        clear_errno();
        let entry = unsafe { libc::readdir(self.stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }))
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.stream) };
    }
}

fn clear_errno() {
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[derive(Clone, Copy)]
pub(super) struct Stat {
    device: u64,
    inode: u64,
    links: u64,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl Stat {
    #[cfg(target_os = "macos")]
    fn from_raw(value: libc::stat) -> Self {
        Self {
            device: value.st_dev as u64,
            inode: value.st_ino,
            links: value.st_nlink as u64,
            mode: u32::from(value.st_mode),
            size: value.st_size as u64,
            modified_seconds: value.st_mtime,
            modified_nanoseconds: value.st_mtime_nsec,
            changed_seconds: value.st_ctime,
            changed_nanoseconds: value.st_ctime_nsec,
        }
    }

    #[cfg(target_os = "linux")]
    fn from_raw(value: libc::stat) -> Self {
        Self {
            device: value.st_dev,
            inode: value.st_ino,
            links: value.st_nlink,
            mode: value.st_mode,
            size: value.st_size as u64,
            modified_seconds: value.st_mtime,
            modified_nanoseconds: value.st_mtime_nsec,
            changed_seconds: value.st_ctime,
            changed_nanoseconds: value.st_ctime_nsec,
        }
    }

    pub(super) fn kind(self) -> u32 {
        self.mode & u32::from(libc::S_IFMT)
    }

    pub(super) fn device(self) -> u64 {
        self.device
    }

    pub(super) fn inode(self) -> u64 {
        self.inode
    }

    pub(super) fn links(self) -> u64 {
        self.links
    }

    pub(super) fn mode(self) -> u32 {
        self.mode
    }

    pub(super) fn size(self) -> u64 {
        self.size
    }

    pub(super) fn same_object(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode && self.kind() == other.kind()
    }

    pub(super) fn same_bound(self, other: Self) -> bool {
        self.same_object(other)
            && self.links == other.links
            && self.mode == other.mode
            && self.size == other.size
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
            && self.changed_seconds == other.changed_seconds
            && self.changed_nanoseconds == other.changed_nanoseconds
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct MountIdentity([u8; 32]);

#[cfg(target_os = "linux")]
pub(super) fn mount_identity(file: &File) -> io::Result<Option<MountIdentity>> {
    const STATX_MNT_ID: u32 = 0x1000;
    let mut statx = MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID == 0 {
        return Ok(None);
    }
    Ok(Some(MountIdentity(
        *blake3::hash(&statx.stx_mnt_id.to_le_bytes()).as_bytes(),
    )))
}

#[cfg(target_os = "macos")]
pub(super) fn mount_identity(file: &File) -> io::Result<Option<MountIdentity>> {
    let mut metadata = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&metadata.f_fsid as *const libc::fsid_t).cast::<u8>(),
            std::mem::size_of::<libc::fsid_t>(),
        )
    };
    Ok(Some(MountIdentity(*blake3::hash(bytes).as_bytes())))
}

pub(super) fn is_symlink_loop(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}
