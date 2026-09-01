use std::process::{Child, Command};

/// Starts a synchronous child as the leader of an isolated process tree.
pub(crate) fn isolate_process_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }
}

/// Terminates a synchronous child and the process tree rooted at it.
///
/// New callers should capture the PID immediately after spawn and use
/// [`terminate_process_tree_with_pid`] so cleanup can still target descendants
/// after the direct child exits.
#[allow(dead_code)]
pub(crate) fn terminate_process_tree(child: &mut Child) {
    let pid = child.id();
    terminate_process_tree_with_pid(child, pid);
}

/// Terminates a synchronous child using the PID captured immediately after spawn.
pub(crate) fn terminate_process_tree_with_pid(child: &mut Child, pid: u32) {
    kill_process_tree(pid);
    let _ = child.kill();
    let _ = child.wait();
}

/// Starts an asynchronous child as the leader of an isolated process tree.
pub(crate) fn isolate_tokio_process_tree(command: &mut tokio::process::Command) {
    isolate_process_tree(command.as_std_mut());
}

/// Terminates an asynchronous child and the process tree rooted at it.
pub(crate) async fn terminate_tokio_process_tree(child: &mut tokio::process::Child, pid: u32) {
    kill_tokio_process_tree(pid).await;
    let _ = child.kill().await;
}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // Children are created as process-group leaders, so a negative PID
        // addresses the complete group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
fn kill_process_tree(pid: u32) {
    let _ = Command::new("taskkill").args(taskkill_args(pid)).status();
}

#[cfg(not(any(unix, windows)))]
fn kill_process_tree(_pid: u32) {}

#[cfg(unix)]
async fn kill_tokio_process_tree(pid: u32) {
    kill_process_tree(pid);
}

#[cfg(windows)]
async fn kill_tokio_process_tree(pid: u32) {
    let _ = tokio::process::Command::new("taskkill")
        .args(taskkill_args(pid))
        .status()
        .await;
}

#[cfg(not(any(unix, windows)))]
async fn kill_tokio_process_tree(_pid: u32) {}

#[cfg(windows)]
fn taskkill_args(pid: u32) -> [String; 4] {
    ["/PID".into(), pid.to_string(), "/T".into(), "/F".into()]
}
