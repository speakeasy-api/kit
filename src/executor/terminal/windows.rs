use std::{
    collections::HashMap,
    fs::File,
    io::{self, Write},
    mem,
    os::windows::io::{FromRawHandle, OwnedHandle},
    process::Command,
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use windows_sys::Win32::{
    Foundation::HANDLE,
    Security::SECURITY_ATTRIBUTES,
    System::{
        Console::{COORD, HPCON},
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Pipes::CreatePipe,
    },
};

use super::{PtyDriver, TerminalError, TerminalId, TerminalOwner, TerminalSize};

type CreatePseudoConsole = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ResizePseudoConsole = unsafe extern "system" fn(HPCON, COORD) -> i32;
type ClosePseudoConsole = unsafe extern "system" fn(HPCON);

#[derive(Clone, Copy)]
struct ConPtyApi {
    create: CreatePseudoConsole,
    resize: ResizePseudoConsole,
    close: ClosePseudoConsole,
}

fn api() -> Option<&'static ConPtyApi> {
    static API: OnceLock<Option<ConPtyApi>> = OnceLock::new();
    API.get_or_init(|| {
        let kernel32 = wide("kernel32.dll");
        // SAFETY: kernel32 is loaded in every Windows process and the strings are NUL terminated.
        let module = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
        if module.is_null() {
            return None;
        }
        // ConPTY was added in Windows 10 1809. Dynamic lookup lets older systems fail closed
        // with PlatformUnavailable instead of failing to load the daemon executable.
        unsafe {
            Some(ConPtyApi {
                create: mem::transmute::<unsafe extern "system" fn() -> isize, CreatePseudoConsole>(
                    GetProcAddress(module, c"CreatePseudoConsole".as_ptr().cast())?,
                ),
                resize: mem::transmute::<unsafe extern "system" fn() -> isize, ResizePseudoConsole>(
                    GetProcAddress(module, c"ResizePseudoConsole".as_ptr().cast())?,
                ),
                close: mem::transmute::<unsafe extern "system" fn() -> isize, ClosePseudoConsole>(
                    GetProcAddress(module, c"ClosePseudoConsole".as_ptr().cast())?,
                ),
            })
        }
    })
    .as_ref()
}

struct ConPty {
    handle: Mutex<Option<HPCON>>,
    input: Mutex<Option<File>>,
    output: Mutex<Option<File>>,
}

impl Drop for ConPty {
    fn drop(&mut self) {
        self.close();
    }
}

impl ConPty {
    fn close(&self) {
        self.input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        output.take();
        drop(output);
        let Some(handle) = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return;
        };
        let Some(api) = api().copied() else {
            return;
        };
        // ClosePseudoConsole is never called on a request thread because Windows may wait for
        // output drain even after the host pipe endpoints have been closed.
        close_on_reaper(api, handle);
    }
}

/// A live pseudoconsole that can be consumed by the Windows Job native launcher.
#[derive(Clone)]
pub struct ConPtyBinding(Arc<ConPty>);

impl ConPtyBinding {
    pub(crate) fn probe(size: TerminalSize) -> io::Result<Self> {
        ensure_reaper(api().ok_or_else(platform_unavailable)?)?;
        allocate(size).map_err(|error| match error {
            TerminalError::Driver(error) => error,
            error => io::Error::other(error.to_string()),
        })
    }

    pub(crate) fn with_handle<T>(&self, operation: impl FnOnce(HPCON) -> T) -> io::Result<T> {
        let handles = self
            .0
            .handle
            .lock()
            .map_err(|_| io::Error::other("ConPTY handle lock poisoned"))?;
        let handle = handles
            .as_ref()
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ConPTY is closed"))?;
        let result = operation(handle);
        drop(handles);
        Ok(result)
    }

    pub(crate) fn close(&self) {
        self.0.close();
    }

    pub fn output_reader(&self) -> io::Result<Box<dyn io::Read + Send>> {
        self.0
            .output
            .lock()
            .map_err(|_| io::Error::other("ConPTY output lock poisoned"))?
            .take()
            .map(|file| Box::new(file) as Box<dyn io::Read + Send>)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "ConPTY output already bound")
            })
    }

    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.0
            .input
            .lock()
            .map_err(|_| io::Error::other("ConPTY input lock poisoned"))?
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ConPTY input is closed"))?
            .write_all(bytes)
    }

    pub fn resize(&self, size: TerminalSize) -> io::Result<()> {
        let api = api().ok_or_else(platform_unavailable)?;
        let size = coord(size)?;
        // The handle lock remains held through ResizePseudoConsole, excluding concurrent close.
        let result = self.with_handle(|handle| unsafe { (api.resize)(handle, size) })?;
        hresult(result)
    }
}

struct ConPtyReaper {
    sender: mpsc::Sender<HPCON>,
    alive: Arc<AtomicBool>,
    retained: Mutex<Vec<HPCON>>,
}

fn reaper(api: ConPtyApi) -> Option<&'static ConPtyReaper> {
    static REAPER: OnceLock<Option<ConPtyReaper>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            let alive = Arc::new(AtomicBool::new(true));
            let worker_alive = alive.clone();
            std::thread::Builder::new()
                .name("conpty-close".to_owned())
                .spawn(move || {
                    struct MarkDead(Arc<AtomicBool>);
                    impl Drop for MarkDead {
                        fn drop(&mut self) {
                            self.0.store(false, Ordering::Release);
                        }
                    }
                    let _mark_dead = MarkDead(worker_alive);
                    for handle in receiver {
                        // SAFETY: each handle is transferred to this worker exactly once.
                        unsafe { (api.close)(handle) };
                    }
                })
                .ok()
                .map(|_| ConPtyReaper {
                    sender,
                    alive,
                    retained: Mutex::new(Vec::new()),
                })
        })
        .as_ref()
}

fn ensure_reaper(api: &ConPtyApi) -> io::Result<()> {
    if reaper(*api).is_some_and(|reaper| reaper_health(&reaper.alive).is_ok()) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "ConPTY nonblocking close worker is unavailable",
        ))
    }
}

fn reaper_health(alive: &AtomicBool) -> io::Result<()> {
    if alive.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "ConPTY nonblocking close worker has stopped",
        ))
    }
}

fn close_on_reaper(api: ConPtyApi, handle: HPCON) {
    let Some(reaper) = reaper(api) else {
        // Process teardown closes retained kernel handles; callers must never block here.
        return;
    };
    let handle = match reaper.sender.send(handle) {
        Ok(()) => return,
        Err(error) => {
            reaper.alive.store(false, Ordering::Release);
            error.0
        }
    };
    reaper
        .retained
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(handle);
}

pub struct NativePtyDriver {
    ptys: Mutex<HashMap<TerminalId, ConPtyBinding>>,
    initialized: bool,
}

impl NativePtyDriver {
    pub fn new() -> Self {
        let initialized = api().is_some_and(|api| ensure_reaper(api).is_ok());
        Self {
            ptys: Mutex::new(HashMap::new()),
            initialized,
        }
    }

    pub fn binding(&self, terminal_id: TerminalId) -> io::Result<ConPtyBinding> {
        self.ptys
            .lock()
            .map_err(|_| io::Error::other("ConPTY map lock poisoned"))?
            .get(&terminal_id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ConPTY is not active"))
    }
}

impl PtyDriver for NativePtyDriver {
    fn ensure_available(&self) -> Result<(), TerminalError> {
        if !self.initialized {
            return Err(TerminalError::PlatformUnavailable);
        }
        ensure_reaper(api().ok_or(TerminalError::PlatformUnavailable)?)
            .map_err(TerminalError::Driver)
    }

    fn allocate(
        &self,
        terminal_id: TerminalId,
        _owner: &TerminalOwner,
        size: TerminalSize,
    ) -> Result<(), TerminalError> {
        let mut ptys = self.ptys.lock().map_err(|_| TerminalError::StatePoisoned)?;
        if ptys.contains_key(&terminal_id) {
            return Err(TerminalError::Driver(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal already owns a ConPTY",
            )));
        }
        self.ensure_available()?;
        let binding = allocate(size)?;
        ptys.insert(terminal_id, binding);
        Ok(())
    }

    fn write_input(&self, terminal_id: TerminalId, bytes: &[u8]) -> io::Result<()> {
        self.binding(terminal_id)?.write_all(bytes)
    }

    fn resize(&self, terminal_id: TerminalId, size: TerminalSize) -> io::Result<()> {
        self.binding(terminal_id)?.resize(size)
    }

    fn interrupt(&self, terminal_id: TerminalId) -> io::Result<()> {
        if let Some(binding) = self
            .ptys
            .lock()
            .map_err(|_| io::Error::other("ConPTY map lock poisoned"))?
            .remove(&terminal_id)
        {
            binding.close();
        }
        Ok(())
    }

    fn bind_process(
        &self,
        _terminal_id: TerminalId,
        _command: &mut Command,
    ) -> io::Result<Box<dyn io::Read + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ConPTY processes must use the suspended Windows Job launcher",
        ))
    }

    fn conpty_binding(&self, terminal_id: TerminalId) -> io::Result<ConPtyBinding> {
        self.binding(terminal_id)
    }
}

fn allocate(size: TerminalSize) -> Result<ConPtyBinding, TerminalError> {
    let api = api().ok_or(TerminalError::PlatformUnavailable)?;
    ensure_reaper(api).map_err(TerminalError::Driver)?;
    let size = coord(size).map_err(TerminalError::Driver)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    };
    let (console_input, host_input) = pipe(&attributes).map_err(TerminalError::Driver)?;
    let (host_output, console_output) = pipe(&attributes).map_err(TerminalError::Driver)?;
    let mut pseudo_console = 0;
    // SAFETY: all four pipe handles are live and the output pointer is valid.
    let result = unsafe {
        (api.create)(
            size,
            console_input.as_raw_handle() as HANDLE,
            console_output.as_raw_handle() as HANDLE,
            0,
            &mut pseudo_console,
        )
    };
    hresult(result).map_err(TerminalError::Driver)?;
    // CreatePseudoConsole duplicates the console-side pipe handles. No pipe handle is inherited
    // by the child; the pseudoconsole attribute is the only child binding.
    drop(console_input);
    drop(console_output);
    Ok(ConPtyBinding(Arc::new(ConPty {
        handle: Mutex::new(Some(pseudo_console)),
        input: Mutex::new(Some(File::from(host_input))),
        output: Mutex::new(Some(File::from(host_output))),
    })))
}

fn pipe(attributes: &SECURITY_ATTRIBUTES) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    // SAFETY: output pointers and SECURITY_ATTRIBUTES are valid. Successful handles are uniquely
    // transferred to OwnedHandle immediately, including on later error paths.
    if unsafe { CreatePipe(&mut read, &mut write, attributes, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read.cast()),
            OwnedHandle::from_raw_handle(write.cast()),
        ))
    }
}

fn hresult(result: i32) -> io::Result<()> {
    if result >= 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result & 0xffff))
    }
}

fn platform_unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "ConPTY is unavailable on this Windows version",
    )
}

fn coord(size: TerminalSize) -> io::Result<COORD> {
    Ok(COORD {
        X: i16::try_from(size.columns)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ConPTY width exceeds i16"))?,
        Y: i16::try_from(size.rows).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "ConPTY height exceeds i16")
        })?,
    })
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

use std::os::windows::io::AsRawHandle;

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

    #[test]
    fn conpty_host_pipe_ends_are_not_inheritable() {
        let binding = allocate(TerminalSize::new(80, 24).unwrap()).unwrap();
        let input = binding.0.input.lock().unwrap();
        let output = binding.0.output.lock().unwrap();
        for handle in [
            input.as_ref().unwrap().as_raw_handle() as HANDLE,
            output.as_ref().unwrap().as_raw_handle() as HANDLE,
        ] {
            let mut flags = 0;
            // SAFETY: each handle is live under its owning mutex guard and flags is writable.
            assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        }
    }

    #[test]
    fn console_and_host_pipe_ends_are_created_non_inheritable() {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 0,
        };
        let first = pipe(&attributes).unwrap();
        let second = pipe(&attributes).unwrap();
        for handle in [
            first.0.as_raw_handle() as HANDLE,
            first.1.as_raw_handle() as HANDLE,
            second.0.as_raw_handle() as HANDLE,
            second.1.as_raw_handle() as HANDLE,
        ] {
            let mut flags = 0;
            assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        }
    }

    #[test]
    fn duplicate_allocation_preserves_the_original_conpty() {
        let driver = NativePtyDriver::new();
        driver.ensure_available().unwrap();
        let terminal = TerminalId::generate().unwrap();
        let owner = TerminalOwner {
            project_id: crate::domain::ids::ProjectId::generate().unwrap(),
            process_id: crate::domain::ids::ProcessId::generate().unwrap(),
            attempt_id: crate::domain::ids::AttemptId::generate().unwrap(),
            principal_id: crate::domain::ids::PrincipalId::generate().unwrap(),
            process_fence: crate::domain::lifecycle::FencingToken::new(1),
            boundary_id: "duplicate-conpty".to_owned(),
        };
        driver
            .allocate(terminal, &owner, TerminalSize::new(80, 24).unwrap())
            .unwrap();
        let original = driver.binding(terminal).unwrap();
        assert!(matches!(
            driver.allocate(terminal, &owner, TerminalSize::new(100, 40).unwrap()),
            Err(TerminalError::Driver(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));
        let current = driver.binding(terminal).unwrap();
        assert!(Arc::ptr_eq(&original.0, &current.0));
        driver.interrupt(terminal).unwrap();
    }

    #[test]
    fn dead_reaper_health_fails_closed_without_closing_on_the_caller() {
        let alive = AtomicBool::new(false);
        assert_eq!(
            reaper_health(&alive).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }
}
