#[cfg(any(target_os = "linux", target_os = "macos"))]
mod supported;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use supported::*;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unavailable;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use unavailable::*;
