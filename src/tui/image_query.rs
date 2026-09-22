//! Synchronous probing before the event reader starts. Picker's query helpers
//! detach a stdin reader on timeout and can later restore stale terminal modes.
use ratatui_image::{
    FontSize,
    picker::{Picker, ProtocolType},
};
#[cfg(unix)]
use std::time::Duration;

pub(super) fn detect() -> Option<Picker> {
    // The caller owns raw mode throughout this synchronous input interval.
    // Construct before probing so tmux passthrough is enabled on first entry.
    // The temporary default is for setup only, never evidence of a known font.
    let fallback = terminal_font_size();
    let initial_size = fallback.unwrap_or(FontSize::new(10, 20));
    let mut picker = picker_with_font_size(initial_size);
    #[allow(unused_mut)] // Only Unix collects query responses.
    let mut detected = Detection::default();
    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
        // Separate open file description: never change stdin's file flags.
        if let Ok(terminal) = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open("/dev/tty")
        {
            let tmux = std::env::var("TERM").is_ok_and(|v| v.starts_with("tmux"))
                || std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "tmux");
            let blacklist = ["WEZTERM_EXECUTABLE", "KONSOLE_VERSION"]
                .iter()
                .any(|key| std::env::var(key).is_ok_and(|v| !v.is_empty()));
            detected = query(&terminal, tmux, blacklist, Duration::from_millis(150));
        }
    }
    // Windows: do not issue queries without a cancellable byte-input API.
    // ConPTY does not reliably return replies. Preserve native iTerm2 environment
    // hints when window-size metadata supplies a font size, without mode changes.
    // As with the original picker fallback, no known font means no native images.
    let size = detected.font_size.or(fallback)?;
    if (size.width, size.height) != (initial_size.width, initial_size.height) {
        picker = picker_with_font_size(size);
    }
    if let Some(protocol) = detected.protocol {
        picker.set_protocol_type(protocol);
    }
    (picker.protocol_type() != ProtocolType::Halfblocks).then_some(picker)
}

#[allow(deprecated)] // Query constructors spawn an unjoinable stdin reader.
fn picker_with_font_size(size: FontSize) -> Picker {
    // This dependency constructor may synchronously wait for `tmux set -p
    // allow-passthrough on`. That preexisting subprocess is outside the query IO
    // deadline; public APIs cannot bypass it while preserving tmux state. It runs
    // before probing and again only if the reported font differs. It never reads
    // stdin or changes terminal modes. Fully bounding this setup needs an upstream
    // side-effect-free constructor; do not bypass it by mutating process env.
    Picker::from_fontsize(size)
}

fn terminal_font_size() -> Option<FontSize> {
    let size = crossterm::terminal::window_size().ok()?;
    let width = size.width.checked_div(size.columns)?;
    let height = size.height.checked_div(size.rows)?;
    (width > 0 && height > 0).then_some(FontSize::new(width, height))
}

#[cfg(unix)]
const MAX_QUERY_BYTES: usize = 4096;
#[derive(Default)]
struct Detection {
    font_size: Option<FontSize>,
    protocol: Option<ProtocolType>,
}

#[cfg(unix)]
fn query(terminal: &std::fs::File, tmux: bool, blacklist: bool, timeout: Duration) -> Detection {
    use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        time::Instant,
    };
    let deadline = Instant::now() + timeout;
    let fd = terminal.as_raw_fd();
    let mut terminal = terminal;
    let mut detected = Detection::default();
    let query = Parser::query(
        tmux,
        QueryStdioOptions {
            timeout,
            blacklist_protocols: if blacklist {
                vec![ProtocolType::Kitty, ProtocolType::Sixel]
            } else {
                vec![]
            },
            ..QueryStdioOptions::default()
        },
    );
    let mut output = query.as_bytes();
    while !output.is_empty() && ready(fd, libc::POLLOUT, deadline) {
        match terminal.write(output) {
            Ok(0) => return detected,
            Ok(n) => output = &output[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => return detected,
        }
    }
    if !output.is_empty() {
        return detected;
    }
    let mut parser = Parser::new();
    // Bound parser allocation/work even on malformed, unterminated replies or
    // perpetually readable input. The absolute monotonic deadline never resets.
    // One-byte reads stop exactly at DSR, leaving subsequent input untouched.
    for _ in 0..MAX_QUERY_BYTES {
        if !ready(fd, libc::POLLIN, deadline) {
            break;
        }
        let mut byte = [0];
        match terminal.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(_) => break,
        }
        for response in parser.push(char::from(byte[0])) {
            match response {
                Response::Status => return detected,
                Response::Kitty if !blacklist => detected.protocol = Some(ProtocolType::Kitty),
                Response::Sixel if !blacklist && detected.protocol.is_none() => {
                    detected.protocol = Some(ProtocolType::Sixel)
                }
                Response::CellSize(Some((w, h))) => detected.font_size = Some(FontSize::new(w, h)),
                _ => {}
            }
        }
    }
    detected
}

// Darwin's /dev/tty alias returns POLLNVAL from poll even though the same
// controlling terminal supports select. Tests on an openpty slave alone do not
// exercise that alias. Keep poll elsewhere, including its high-fd support.
#[cfg(target_os = "macos")]
fn ready(fd: std::os::fd::RawFd, events: libc::c_short, deadline: std::time::Instant) -> bool {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return false;
    }
    loop {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return false;
        };
        if remaining.is_zero() {
            return false;
        }
        // SAFETY: zero is a valid empty fd_set; fd was range-checked before
        // FD_SET. select receives only live initialized sets and a timeval.
        let result = unsafe {
            let mut reads: libc::fd_set = std::mem::zeroed();
            let mut writes: libc::fd_set = std::mem::zeroed();
            if events & libc::POLLIN != 0 {
                libc::FD_SET(fd, &mut reads);
            }
            if events & libc::POLLOUT != 0 {
                libc::FD_SET(fd, &mut writes);
            }
            let mut timeout = libc::timeval {
                tv_sec: remaining.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
                tv_usec: remaining.subsec_micros() as libc::suseconds_t,
            };
            libc::select(
                fd + 1,
                &mut reads,
                &mut writes,
                std::ptr::null_mut(),
                &mut timeout,
            )
        };
        if result >= 0 {
            return result > 0 && std::time::Instant::now() < deadline;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn ready(fd: std::os::fd::RawFd, events: libc::c_short, deadline: std::time::Instant) -> bool {
    loop {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return false;
        };
        if remaining.is_zero() {
            return false;
        }
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let millis = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        // SAFETY: poll points at one initialized, live pollfd.
        let result = unsafe { libc::poll(&mut poll, 1, millis) };
        if result >= 0 {
            return result > 0
                && poll.revents & events != 0
                && std::time::Instant::now() < deadline;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

#[cfg(all(test, unix))]
#[path = "image_query_tests.rs"]
mod tests;
