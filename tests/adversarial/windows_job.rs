#![cfg(windows)]

use std::{
    os::windows::process::CommandExt,
    process::Command,
    time::{Duration, Instant},
};

use kit::executor::{
    backends::windows_job::{Job, WindowsCommand},
    process::tree::{BoundaryControl, Ownership},
    profile::ResourceLimits,
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NOT_ENOUGH_QUOTA};
use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;

fn limits(pids: u32) -> ResourceLimits {
    ResourceLimits::new(30_000, 512 * 1024 * 1024, pids, 1, 1, 1, 4096, 30_000)
}

#[test]
fn breakaway_child_is_denied() {
    if std::env::var_os("KIT_WINDOWS_BREAKAWAY_CHILD").is_some() {
        let error = Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/d", "/c", "exit 0"])
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
            .status()
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        return;
    }

    let owner = Ownership::new("kit-adversarial", "breakaway:fence:1").unwrap();
    let mut job = Job::create(owner.clone(), limits(8)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("windows_job::breakaway_child_is_denied")
                .arg("--nocapture")
                .env("KIT_WINDOWS_BREAKAWAY_CHILD", "1"),
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        job.wait(&process, Instant::now() + Duration::from_secs(10))
            .unwrap(),
        0
    );
    assert_eq!(job.active_processes().unwrap(), 0);
}

#[test]
fn nested_descendants_are_killed_without_survivors() {
    let owner = Ownership::new("kit-adversarial", "descendants:fence:1").unwrap();
    let mut job = Job::create(owner.clone(), limits(8)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(r"C:\Windows\System32\cmd.exe")
                .arg("/d")
                .arg("/c")
                .arg("start /b cmd /d /c ping -n 30 127.0.0.1 ^>nul & ping -n 30 127.0.0.1 >nul"),
            None,
            |_| Ok(()),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while job.active_processes().unwrap() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(job.active_processes().unwrap() >= 2);
    job.kill_boundary(Instant::now() + Duration::from_secs(5))
        .unwrap();
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(job.active_processes().unwrap(), 0);
    let _ = process.wait(Instant::now() + Duration::from_secs(5));
}

#[test]
fn active_process_limit_denies_an_extra_descendant() {
    if std::env::var_os("KIT_WINDOWS_PID_LIMIT_CHILD").is_some() {
        let error = Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/d", "/c", "exit 0"])
            .status()
            .unwrap_err();
        assert!(matches!(
            error.raw_os_error(),
            Some(code)
                if code == ERROR_ACCESS_DENIED as i32 || code == ERROR_NOT_ENOUGH_QUOTA as i32
        ));
        return;
    }

    let owner = Ownership::new("kit-adversarial", "pid-limit:fence:1").unwrap();
    let mut job = Job::create(owner.clone(), limits(1)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("windows_job::active_process_limit_denies_an_extra_descendant")
                .arg("--nocapture")
                .env("KIT_WINDOWS_PID_LIMIT_CHILD", "1"),
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        job.wait(&process, Instant::now() + Duration::from_secs(10))
            .unwrap(),
        0
    );
    assert_eq!(job.active_processes().unwrap(), 0);
}
