use std::{
    fs::File,
    io::{self, Seek, Write},
    os::unix::io::FromRawFd,
    process::Command,
};

#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::raw::c_char;
use std::os::unix::process::CommandExt;

const FD_CLOEXEC: i32 = 1;
#[cfg(target_os = "linux")]
const F_DUPFD_CLOEXEC: i32 = 1030;
#[cfg(target_os = "macos")]
const F_DUPFD_CLOEXEC: i32 = 67;
const F_SETFD: i32 = 2;
const MAX_DESCRIPTOR_SCAN: u64 = 1_048_576;

pub(super) fn descriptor_file(value: &[u8]) -> io::Result<File> {
    let descriptor = create_anonymous_file()?;
    // SAFETY: create_anonymous_file returns a new owned CLOEXEC descriptor.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let length = i64::try_from(value.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "secret is too large"))?;
    if unsafe { ftruncate(descriptor, length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    file.write_all(value)?;
    file.flush()?;
    file.rewind()?;
    restrict_file(descriptor)?;
    seal_file(descriptor)?;
    Ok(file)
}

pub(super) fn configure_allowlist(command: &mut Command, mut mappings: Vec<(i32, i32)>) {
    // SAFETY: the closure invokes only async-signal-safe descriptor operations.
    unsafe {
        command.pre_exec(move || {
            mark_unrelated_close_on_exec()?;
            let safe_descriptor = mappings
                .iter()
                .map(|&(_, target)| target)
                .max()
                .unwrap_or(2)
                .checked_add(1)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "secret descriptor range overflow",
                    )
                })?;
            for (source, _) in &mut mappings {
                let duplicate = fcntl(*source, F_DUPFD_CLOEXEC, safe_descriptor);
                if duplicate == -1 {
                    return Err(io::Error::last_os_error());
                }
                *source = duplicate;
            }
            for &(source, target) in &mappings {
                if dup2(source, target) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if fcntl(target, F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(target_os = "linux")]
fn create_anonymous_file() -> io::Result<i32> {
    const MFD_CLOEXEC: u32 = 1;
    const MFD_ALLOW_SEALING: u32 = 2;
    let name = b"kit-secret\0";
    // SAFETY: name is NUL-terminated and flags have no pointer arguments.
    let descriptor = unsafe {
        memfd_create(
            name.as_ptr().cast::<c_char>(),
            MFD_CLOEXEC | MFD_ALLOW_SEALING,
        )
    };
    if descriptor == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(descriptor)
    }
}

#[cfg(target_os = "macos")]
fn create_anonymous_file() -> io::Result<i32> {
    use std::os::unix::{fs::OpenOptionsExt, io::IntoRawFd};

    for _ in 0..8 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let name = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!(".kit-secret-{name}"));
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                fs::remove_file(path)?;
                return Ok(file.into_raw_fd());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a private anonymous file",
    ))
}

#[cfg(target_os = "linux")]
fn seal_file(descriptor: i32) -> io::Result<()> {
    const F_ADD_SEALS: i32 = 1033;
    const F_SEAL_SEAL: i32 = 1;
    const F_SEAL_SHRINK: i32 = 2;
    const F_SEAL_GROW: i32 = 4;
    const F_SEAL_WRITE: i32 = 8;
    if unsafe {
        fcntl(
            descriptor,
            F_ADD_SEALS,
            F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE,
        )
    } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn restrict_file(descriptor: i32) -> io::Result<()> {
    if unsafe { fchmod(descriptor, 0o400) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn seal_file(_descriptor: i32) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn restrict_file(_descriptor: i32) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn mark_unrelated_close_on_exec() -> io::Result<()> {
    const CLOSE_RANGE_CLOEXEC: u32 = 4;
    const SYS_CLOSE_RANGE: isize = 436;
    // SAFETY: close_range changes descriptor flags only in the forked child.
    if unsafe { syscall(SYS_CLOSE_RANGE, 3_u32, u32::MAX, CLOSE_RANGE_CLOEXEC) } == 0 {
        return Ok(());
    }
    mark_by_scan(7)
}

#[cfg(target_os = "macos")]
fn mark_unrelated_close_on_exec() -> io::Result<()> {
    mark_by_scan(8)
}

fn mark_by_scan(resource: i32) -> io::Result<()> {
    let mut limit = RLimit {
        current: 0,
        maximum: 0,
    };
    if unsafe { getrlimit(resource, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if limit.current > MAX_DESCRIPTOR_SCAN {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor limit is too large for a fail-closed allowlist scan",
        ));
    }
    for descriptor in 3..limit.current as i32 {
        if unsafe { fcntl(descriptor, F_SETFD, FD_CLOEXEC) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(9) {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[repr(C)]
struct RLimit {
    current: u64,
    maximum: u64,
}

unsafe extern "C" {
    fn dup2(source: i32, target: i32) -> i32;
    fn fcntl(descriptor: i32, command: i32, ...) -> i32;
    fn ftruncate(descriptor: i32, length: i64) -> i32;
    fn getrlimit(resource: i32, limits: *mut RLimit) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn descriptor_remap_preserves_overlap_cycles() {
        let files = [
            b"ninety-nine".as_slice(),
            b"one-hundred".as_slice(),
            b"one-oh-one".as_slice(),
        ]
        .map(|value| descriptor_file(value).unwrap());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat <&99; cat <&100; cat <&101"]);
        let descriptors = files.each_ref().map(|file| file.as_raw_fd());
        // SAFETY: the setup runs after fork and only duplicates live captured descriptors.
        unsafe {
            command.pre_exec(move || {
                for (source, target) in descriptors.into_iter().zip(99..=101) {
                    if dup2(source, target) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        configure_allowlist(&mut command, vec![(99, 100), (100, 101), (101, 99)]);

        let output = command.output().unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, b"one-oh-oneninety-nineone-hundred");
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn fchmod(descriptor: i32, mode: u32) -> i32;
    fn memfd_create(name: *const c_char, flags: u32) -> i32;
    fn syscall(number: isize, ...) -> isize;
}
