//! Colours and glyphs for the terminal client.
//!
//! Every colour is drawn on the terminal's own background, which the client
//! never paints over, so a palette built for a dark terminal washes out to
//! nothing on a light one. The palette is therefore chosen once at startup
//! from the background the terminal reports, and every call site asks for a
//! role — `text`, `dim`, `accent` — rather than a fixed colour.

use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

use ratatui::style::{Color, Modifier, Style};

/// Which way round the terminal's colours run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// One colour per role the client draws in.
struct Palette {
    accent: Color,
    user: Color,
    running: Color,
    success: Color,
    warn: Color,
    error: Color,
    dim: Color,
    faint: Color,
    text: Color,
    code_fg: Color,
    code_bg: Color,
    bar_bg: Color,
}

const DARK: Palette = Palette {
    accent: Color::Rgb(94, 205, 190),
    user: Color::Rgb(137, 180, 250),
    running: Color::Rgb(197, 168, 250),
    success: Color::Rgb(126, 200, 130),
    warn: Color::Rgb(230, 180, 100),
    error: Color::Rgb(240, 120, 120),
    dim: Color::Rgb(122, 130, 145),
    faint: Color::Rgb(78, 85, 98),
    text: Color::Rgb(215, 220, 228),
    code_fg: Color::Rgb(232, 200, 140),
    code_bg: Color::Rgb(30, 33, 41),
    bar_bg: Color::Rgb(24, 27, 34),
};

/// The same roles at the same hues, darkened to carry on a light terminal.
const LIGHT: Palette = Palette {
    accent: Color::Rgb(0, 102, 93),
    user: Color::Rgb(26, 86, 190),
    running: Color::Rgb(103, 58, 183),
    success: Color::Rgb(24, 110, 60),
    warn: Color::Rgb(132, 78, 0),
    error: Color::Rgb(186, 32, 40),
    dim: Color::Rgb(88, 96, 112),
    faint: Color::Rgb(128, 136, 152),
    text: Color::Rgb(32, 36, 44),
    code_fg: Color::Rgb(124, 72, 16),
    code_bg: Color::Rgb(234, 236, 240),
    bar_bg: Color::Rgb(216, 220, 228),
};

/// The palette in force, as an [`Appearance`] discriminant.
///
/// Dark is the default: it is what every terminal that answers no question at
/// all gets, and what the client drew with before light terminals were read.
static CHOSEN: AtomicU8 = AtomicU8::new(DARK_CODE);

const DARK_CODE: u8 = 0;
const LIGHT_CODE: u8 = 1;

/// How long the terminal gets to answer the background query.
///
/// Terminals that do not implement it answer nothing, so this is dead time on
/// startup for them and has to stay short enough not to be felt.
const PROBE: Duration = Duration::from_millis(100);

/// Reads the terminal's background and fixes the palette for the session.
///
/// Called once before the alternate screen is entered, while the terminal can
/// still be asked a question and answer it on stdin.
pub fn detect() {
    set(detected());
}

pub fn set(appearance: Appearance) {
    CHOSEN.store(
        match appearance {
            Appearance::Dark => DARK_CODE,
            Appearance::Light => LIGHT_CODE,
        },
        Ordering::Relaxed,
    );
}

pub fn appearance() -> Appearance {
    if CHOSEN.load(Ordering::Relaxed) == LIGHT_CODE {
        Appearance::Light
    } else {
        Appearance::Dark
    }
}

fn palette() -> &'static Palette {
    palette_of(appearance())
}

fn palette_of(appearance: Appearance) -> &'static Palette {
    match appearance {
        Appearance::Dark => &DARK,
        Appearance::Light => &LIGHT,
    }
}

fn detected() -> Appearance {
    if let Some(forced) = std::env::var("KIT_THEME").ok().as_deref().and_then(forced) {
        return forced;
    }
    if let Some(background) = probe_background(PROBE) {
        return appearance_of(background);
    }
    std::env::var("COLORFGBG")
        .ok()
        .as_deref()
        .and_then(from_colorfgbg)
        .unwrap_or(Appearance::Dark)
}

/// An explicit choice, for terminals that answer wrongly or not at all.
fn forced(value: &str) -> Option<Appearance> {
    match value.trim().to_ascii_lowercase().as_str() {
        "light" => Some(Appearance::Light),
        "dark" => Some(Appearance::Dark),
        _ => None,
    }
}

/// The appearance implied by a background colour.
fn appearance_of((red, green, blue): (u8, u8, u8)) -> Appearance {
    let luminance = 0.2126 * f64::from(red) + 0.7152 * f64::from(green) + 0.0722 * f64::from(blue);
    if luminance > 127.5 {
        Appearance::Light
    } else {
        Appearance::Dark
    }
}

/// `COLORFGBG`, the pre-OSC convention: foreground and background as palette
/// indices, sometimes with a cursor colour wedged between them.
fn from_colorfgbg(value: &str) -> Option<Appearance> {
    let background: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    // Indices 0-6 and 8 are the dark half of the ANSI palette; 7 and 9-15 are
    // the light half, and anything above that is the 256-colour cube, where
    // the greyscale ramp ends light.
    match background {
        0..=6 | 8 => Some(Appearance::Dark),
        7 | 9..=15 => Some(Appearance::Light),
        _ => None,
    }
}

/// Pulls the background out of an OSC 11 reply, once the reply is complete.
///
/// Terminals answer `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` closed by either BEL or
/// ST, with components of one to four hex digits. An answer still in flight
/// reads as no answer, so the caller keeps waiting rather than parsing half a
/// colour.
fn parse_background(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(reply);
    let body = text.split("rgb:").nth(1)?;
    let end = terminator(body)?;
    let mut components = body[..end].split('/');
    let color = (
        component(components.next()?)?,
        component(components.next()?)?,
        component(components.next()?)?,
    );
    components.next().is_none().then_some(color)
}

/// Where the answer ends: BEL, or ST written out in full as `ESC` `\\`.
///
/// A reply that stops on a lone `ESC` is still arriving. Ending it there would
/// take the colour but leave the backslash behind, and the event loop would
/// then read it as the first thing the user typed.
fn terminator(body: &str) -> Option<usize> {
    body.find('\x07')
        .into_iter()
        .chain(body.find("\x1b\\"))
        .min()
}

/// One hex component of an OSC colour, scaled to eight bits.
fn component(digits: &str) -> Option<u8> {
    let width = digits.len();
    if !(1..=4).contains(&width) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let full = u32::from(u16::MAX);
    // Widen to 16 bits by repeating the digits, the way X11 reads short specs,
    // then narrow to the eight bits a terminal colour is written in.
    let widened = value * (full / (16u32.pow(width as u32) - 1));
    Some((widened >> 8) as u8)
}

/// Asks the terminal for its background colour over OSC 11.
///
/// The query goes to the controlling terminal rather than stdout so a client
/// with its output redirected still reaches the terminal, and is asked with
/// the terminal in raw mode, since a cooked terminal holds the reply back
/// until the user presses return.
#[cfg(unix)]
fn probe_background(timeout: Duration) -> Option<(u8, u8, u8)> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    // Without raw mode the terminal holds the reply back until the user
    // presses return, so an unaskable terminal is left alone entirely.
    crossterm::terminal::enable_raw_mode().ok()?;
    let background = ask(&mut tty, timeout);
    let _ = crossterm::terminal::disable_raw_mode();
    background
}

/// Puts the OSC 11 question to a terminal and waits out its answer.
///
/// Whatever the terminal says arrives in pieces, so the reply is read until it
/// parses or the deadline passes: a terminal that does not implement the query
/// says nothing at all, and must not hold up startup for longer than that.
#[cfg(unix)]
fn ask(tty: &mut std::fs::File, timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        time::Instant,
    };

    tty.write_all(b"\x1b]11;?\x1b\\").ok()?;
    tty.flush().ok()?;
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut poll = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialised descriptor, owned by `tty` and open for the
        // whole call, waited on for a bounded time.
        let ready = unsafe { libc::poll(&raw mut poll, 1, left.as_millis() as i32) };
        // A signal cutting the wait short is not an answer either way, and
        // the deadline still bounds what is left of the wait.
        if ready < 0 && interrupted() {
            continue;
        }
        if ready <= 0 {
            return None;
        }
        let mut chunk = [0u8; 64];
        match tty.read(&mut chunk) {
            Ok(0) => return None,
            Ok(read) => reply.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
        if let Some(background) = parse_background(&reply) {
            return Some(background);
        }
        // A terminal that answers something else — or a keystroke that arrives
        // first — must not keep the client waiting either.
        if reply.len() > 256 {
            return None;
        }
    }
}

/// Whether the last `poll` failed only because a signal arrived.
#[cfg(unix)]
fn interrupted() -> bool {
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
}

#[cfg(not(unix))]
fn probe_background(_timeout: Duration) -> Option<(u8, u8, u8)> {
    None
}

/// The WCAG contrast ratio between two true colours.
///
/// Every palette entry is checked against the background it is drawn on, so
/// the ratio is what the tests assert on rather than the colours themselves.
#[cfg(test)]
pub fn contrast(color: Color, background: Color) -> f64 {
    fn luminance(color: Color) -> f64 {
        let Color::Rgb(red, green, blue) = color else {
            panic!("the client draws in true colour");
        };
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
    }
    let (one, two) = (luminance(color), luminance(background));
    (one.max(two) + 0.05) / (one.min(two) + 0.05)
}

/// Activity indicators, one per kind of work.
///
/// Several of these can be on screen at once — a turn, the tool it is running,
/// and each nested dispatch — so they animate with different shapes and speeds
/// instead of beating in unison. Every frame is one cell wide.
#[derive(Clone, Copy)]
pub enum Pulse {
    /// The turn as a whole.
    Turn,
    /// A model-visible tool call.
    Tool,
    /// One nested dispatch inside a compose run.
    Child,
    /// The status bar's heartbeat.
    Status,
}

impl Pulse {
    const fn frames(self) -> &'static [&'static str] {
        match self {
            Self::Turn => &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"],
            Self::Tool => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            Self::Child => &["⠁", "⠂", "⠄", "⡀", "⢀", "⠠", "⠐", "⠈"],
            Self::Status => &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "▆", "▅", "▄", "▃", "▂"],
        }
    }

    /// Animation ticks per frame; slower indicators read as calmer.
    const fn every(self) -> usize {
        match self {
            Self::Turn | Self::Tool => 1,
            Self::Child => 2,
            Self::Status => 3,
        }
    }
}

pub fn accent_color() -> Color {
    palette().accent
}

pub fn user_color() -> Color {
    palette().user
}

pub fn running_color() -> Color {
    palette().running
}

pub fn success_color() -> Color {
    palette().success
}

pub fn warn_color() -> Color {
    palette().warn
}

pub fn error_color() -> Color {
    palette().error
}

pub fn text_color() -> Color {
    palette().text
}

pub fn bar_bg() -> Color {
    palette().bar_bg
}

pub fn text() -> Style {
    Style::default().fg(palette().text)
}

pub fn dim() -> Style {
    Style::default().fg(palette().dim)
}

pub fn faint() -> Style {
    Style::default().fg(palette().faint)
}

pub fn accent() -> Style {
    Style::default().fg(palette().accent)
}

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub fn code() -> Style {
    Style::default().fg(palette().code_fg).bg(palette().code_bg)
}

pub fn bar() -> Style {
    Style::default().fg(palette().dim).bg(palette().bar_bg)
}

/// The frame this indicator shows on an animation tick.
pub fn pulse(kind: Pulse, tick: usize) -> &'static str {
    let frames = kind.frames();
    frames[(tick / kind.every()) % frames.len()]
}

/// Human-readable elapsed time, sized to the magnitude.
pub fn duration(millis: u64) -> String {
    if millis < 1_000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", millis as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", millis / 60_000, (millis % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use std::time::Duration;

    use super::{
        Appearance, DARK, LIGHT, Palette, appearance_of, contrast, forced, from_colorfgbg,
        palette_of, parse_background,
    };

    fn roles(palette: &'static Palette) -> [(&'static str, Color); 8] {
        [
            ("accent", palette.accent),
            ("user", palette.user),
            ("running", palette.running),
            ("success", palette.success),
            ("warn", palette.warn),
            ("error", palette.error),
            ("dim", palette.dim),
            ("text", palette.text),
        ]
    }

    #[test]
    fn dark_palette_is_unchanged() {
        assert_eq!(DARK.text, Color::Rgb(215, 220, 228));
        assert_eq!(DARK.accent, Color::Rgb(94, 205, 190));
        assert_eq!(DARK.dim, Color::Rgb(122, 130, 145));
        assert_eq!(DARK.faint, Color::Rgb(78, 85, 98));
        assert_eq!(DARK.code_bg, Color::Rgb(30, 33, 41));
        assert_eq!(DARK.bar_bg, Color::Rgb(24, 27, 34));
    }

    #[test]
    fn light_palette_carries_on_a_white_terminal() {
        let white = Color::Rgb(255, 255, 255);
        for (role, color) in roles(&LIGHT) {
            let contrast = contrast(color, white);
            assert!(contrast >= 4.5, "{role} is {contrast:.2}:1 on white");
        }
        // Faint is meant to recede, but not below the dark palette's own
        // faintest reading, which is legible on a black terminal.
        assert!(contrast(LIGHT.faint, white) >= contrast(DARK.faint, Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn dark_palette_carries_on_a_black_terminal() {
        let black = Color::Rgb(0, 0, 0);
        for (role, color) in roles(&DARK) {
            let contrast = contrast(color, black);
            assert!(contrast >= 4.5, "{role} is {contrast:.2}:1 on black");
        }
    }

    #[test]
    fn palette_chrome_stays_legible_against_its_own_fills() {
        for (name, palette) in [("dark", &DARK), ("light", &LIGHT)] {
            let bar = contrast(palette.dim, palette.bar_bg);
            assert!(bar >= 4.0, "{name} status bar is {bar:.2}:1");
            let code = contrast(palette.code_fg, palette.code_bg);
            assert!(code >= 4.5, "{name} inline code is {code:.2}:1");
        }
    }

    #[test]
    fn background_luminance_picks_the_palette() {
        assert_eq!(appearance_of((255, 255, 255)), Appearance::Light);
        assert_eq!(appearance_of((250, 246, 236)), Appearance::Light);
        assert_eq!(appearance_of((0, 0, 0)), Appearance::Dark);
        assert_eq!(appearance_of((30, 33, 41)), Appearance::Dark);
        // Solarized: light and dark share a hue and differ only in level.
        assert_eq!(appearance_of((253, 246, 227)), Appearance::Light);
        assert_eq!(appearance_of((0, 43, 54)), Appearance::Dark);
    }

    #[test]
    fn reads_a_terminals_osc_reply() {
        assert_eq!(
            parse_background(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some((255, 255, 255))
        );
        assert_eq!(
            parse_background(b"\x1b]11;rgb:1e1e/2121/2929\x07"),
            Some((30, 33, 41))
        );
        // Short components are widened the way X11 reads them.
        assert_eq!(
            parse_background(b"\x1b]11;rgb:f/0/0\x07"),
            Some((255, 0, 0))
        );
        assert_eq!(
            parse_background(b"\x1b]11;rgb:ff/80/00\x07"),
            Some((255, 128, 0))
        );
    }

    #[test]
    fn waits_out_a_reply_that_is_still_arriving() {
        assert_eq!(parse_background(b"\x1b]11;rgb:ffff/ff"), None);
        assert_eq!(parse_background(b""), None);
        assert_eq!(parse_background(b"\x1b]11;rgb:ffff/ffff\x07"), None);
        assert_eq!(parse_background(b"\x1b]11;rgb:zz/zz/zz\x07"), None);
        assert_eq!(parse_background(b"\x1b]11;rgb:1/2/3/4\x07"), None);
    }

    /// Reading the colour off a half-arrived ST would leave the backslash in
    /// the terminal's buffer, where the event loop reads it as a keystroke.
    #[test]
    fn holds_out_for_the_whole_terminator() {
        let split = b"\x1b]11;rgb:1e1e/2121/2929\x1b";
        assert_eq!(parse_background(split), None);
        let mut whole = split.to_vec();
        whole.push(b'\\');
        assert_eq!(parse_background(&whole), Some((30, 33, 41)));
    }

    /// A stand-in terminal on one end of a socket pair: the probe writes its
    /// query into it and reads the answer back, exactly as it would from a
    /// terminal, without needing this test to own one.
    #[cfg(unix)]
    fn terminal() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;

        let mut ends = [0; 2];
        // SAFETY: a stream socket pair, whose two descriptors are handed
        // straight to `File` and closed when those are dropped.
        let made =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, ends.as_mut_ptr()) };
        assert_eq!(made, 0, "socketpair");
        // SAFETY: both descriptors are fresh and owned by nothing else.
        unsafe {
            (
                std::fs::File::from_raw_fd(ends[0]),
                std::fs::File::from_raw_fd(ends[1]),
            )
        }
    }

    #[cfg(unix)]
    #[test]
    fn asks_the_terminal_and_waits_for_its_answer() {
        use std::io::{Read, Write};

        let (mut client, mut terminal) = terminal();
        let answering = std::thread::spawn(move || {
            let mut query = [0; 16];
            let read = terminal.read(&mut query).expect("query arrives");
            assert_eq!(&query[..read], b"\x1b]11;?\x1b\\");
            // Terminals answer in whatever pieces they please.
            terminal
                .write_all(b"\x1b]11;rgb:fdfd/f6f6")
                .expect("first half");
            terminal.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(20));
            terminal.write_all(b"/e3e3\x1b").expect("second half");
            terminal.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(20));
            terminal.write_all(b"\\").expect("terminator");
            terminal.flush().expect("flush");
            terminal
        });
        assert_eq!(
            super::ask(&mut client, Duration::from_secs(5)),
            Some((253, 246, 227))
        );
        assert_eq!(appearance_of((253, 246, 227)), Appearance::Light);
        drop(answering.join().expect("the terminal answers"));
    }

    #[cfg(unix)]
    #[test]
    fn gives_up_on_a_terminal_that_never_answers() {
        let (mut client, terminal) = terminal();
        let started = std::time::Instant::now();
        assert_eq!(super::ask(&mut client, Duration::from_millis(50)), None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "startup stalled"
        );
        drop(terminal);
    }

    #[test]
    fn reads_the_colorfgbg_convention() {
        assert_eq!(from_colorfgbg("15;0"), Some(Appearance::Dark));
        assert_eq!(from_colorfgbg("0;15"), Some(Appearance::Light));
        assert_eq!(from_colorfgbg("0;default;15"), Some(Appearance::Light));
        assert_eq!(from_colorfgbg("15;default;0"), Some(Appearance::Dark));
        assert_eq!(from_colorfgbg("0;7"), Some(Appearance::Light));
        assert_eq!(from_colorfgbg("7;default"), None);
        assert_eq!(from_colorfgbg(""), None);
    }

    #[test]
    fn each_appearance_draws_from_its_own_palette() {
        assert_eq!(palette_of(Appearance::Dark).text, DARK.text);
        assert_eq!(palette_of(Appearance::Light).text, LIGHT.text);
        assert_ne!(DARK.text, LIGHT.text);
        assert_ne!(DARK.code_bg, LIGHT.code_bg);
    }

    #[test]
    fn an_explicit_choice_overrides_the_terminal() {
        assert_eq!(forced("light"), Some(Appearance::Light));
        assert_eq!(forced(" DARK "), Some(Appearance::Dark));
        assert_eq!(forced("auto"), None);
        assert_eq!(forced(""), None);
    }
}
