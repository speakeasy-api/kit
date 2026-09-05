use kit::resilient_fs::{
    self, Backend, BackendFile, BackendLease, DiskBackend, DiskEntry, DiskOpenOptions, LeaseRequest,
};
use std::{
    fs, io,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
pub struct Capacity {
    pub exhausted: AtomicBool,
    pub exhaust_on_write: AtomicBool,
    pub repaired: PathBuf,
}
impl Capacity {
    fn check(&self) -> io::Result<()> {
        if self.exhausted.load(Ordering::SeqCst) && !self.repaired.exists() {
            Err(io::ErrorKind::StorageFull.into())
        } else {
            Ok(())
        }
    }
}
pub struct CapacityDisk(pub Arc<Capacity>);
struct CapacityFile {
    inner: Box<dyn BackendFile>,
    capacity: Arc<Capacity>,
}
impl Read for CapacityFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.inner.read(bytes)
    }
}
impl Seek for CapacityFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.inner.seek(from)
    }
}
impl Write for CapacityFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.capacity.exhaust_on_write.swap(false, Ordering::SeqCst) {
            self.capacity.exhausted.store(true, Ordering::SeqCst);
        }
        self.capacity.check()?;
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.capacity.check()?;
        self.inner.flush()
    }
}
impl BackendFile for CapacityFile {
    fn identity(&self) -> io::Result<Option<resilient_fs::FileIdentity>> {
        self.inner.identity()
    }
    fn metadata(&self) -> io::Result<fs::Metadata> {
        self.inner.metadata()
    }
    fn set_len(&self, size: u64) -> io::Result<()> {
        self.capacity.check()?;
        self.inner.set_len(size)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.capacity.check()?;
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.capacity.check()?;
        self.inner.sync_all()
    }
    fn set_permissions(&self, p: fs::Permissions) -> io::Result<()> {
        self.capacity.check()?;
        self.inner.set_permissions(p)
    }
}
impl Backend for CapacityDisk {
    fn identity(
        &self,
        path: &Path,
        follow: bool,
    ) -> io::Result<Option<resilient_fs::FileIdentity>> {
        DiskBackend.identity(path, follow)
    }
    fn open(&self, path: &Path, options: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        if options.write
            || options.append
            || options.create
            || options.create_new
            || options.truncate
        {
            self.0.check()?;
        }
        Ok(Box::new(CapacityFile {
            inner: DiskBackend.open(path, options)?,
            capacity: self.0.clone(),
        }))
    }
    fn metadata(&self, path: &Path, follow: bool) -> io::Result<fs::Metadata> {
        DiskBackend.metadata(path, follow)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DiskEntry>> {
        DiskBackend.read_dir(path)
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        DiskBackend.read_link(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        DiskBackend.canonicalize(path)
    }
    fn create_dir(&self, path: &Path, private: bool) -> io::Result<()> {
        self.0.check()?;
        DiskBackend.create_dir(path, private)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.0.check()?;
        DiskBackend.remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.0.check()?;
        DiskBackend.remove_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.0.check()?;
        DiskBackend.rename(from, to)
    }
    fn set_permissions(&self, path: &Path, p: fs::Permissions) -> io::Result<()> {
        self.0.check()?;
        DiskBackend.set_permissions(path, p)
    }
    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        self.0.check()?;
        DiskBackend.sync_directory(path)
    }
    fn acquire_lease(&self, request: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
        self.0.check()?;
        DiskBackend.acquire_lease(request)
    }
    fn open_beneath(&self, root: &Path, relative: &Path) -> io::Result<Box<dyn BackendFile>> {
        Ok(Box::new(CapacityFile {
            inner: DiskBackend.open_beneath(root, relative)?,
            capacity: self.0.clone(),
        }))
    }
}
