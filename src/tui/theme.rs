//! Colours and glyphs for the terminal client.
//!
//! Functional UI colours use the terminal's defaults and ANSI palette, so the
//! client follows the user's terminal theme. Speakeasy's true-colour ramp is
//! reserved for decorative brand moments and falls back to ANSI colours when
//! the terminal does not advertise 24-bit colour.

use ratatui::style::{Color, Modifier, Style};

const BRAND_RGB: [Color; 9] = [
    Color::Rgb(0x32, 0x0f, 0x1e),
    Color::Rgb(0xc8, 0x32, 0x28),
    Color::Rgb(0xfb, 0x87, 0x3f),
    Color::Rgb(0xd2, 0xdc, 0x91),
    Color::Rgb(0x5a, 0x82, 0x50),
    Color::Rgb(0x00, 0x23, 0x14),
    Color::Rgb(0x00, 0x14, 0x3c),
    Color::Rgb(0x28, 0x73, 0xd7),
    Color::Rgb(0x9b, 0xc3, 0xff),
];

const BRAND_ANSI: [Color; 9] = [
    Color::Magenta,
    Color::Red,
    Color::Yellow,
    Color::Yellow,
    Color::Green,
    Color::Green,
    Color::Blue,
    Color::Blue,
    Color::Cyan,
];

/// Speakeasy's warm-to-cool brand ramp, with a terminal-palette fallback.
pub fn brand_rainbow() -> &'static [Color; 9] {
    brand_rainbow_for(crossterm::style::available_color_count())
}

fn brand_rainbow_for(color_count: u16) -> &'static [Color; 9] {
    if color_count == u16::MAX {
        &BRAND_RGB
    } else {
        &BRAND_ANSI
    }
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

pub const fn accent_color() -> Color {
    Color::Cyan
}

pub const fn user_color() -> Color {
    Color::Blue
}

pub const fn running_color() -> Color {
    Color::Cyan
}

pub const fn success_color() -> Color {
    Color::Green
}

pub const fn warn_color() -> Color {
    Color::Yellow
}

pub const fn error_color() -> Color {
    Color::Red
}

pub const fn text_color() -> Color {
    Color::Reset
}

pub fn text() -> Style {
    Style::default()
}

pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn faint() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub fn accent() -> Style {
    Style::default().fg(accent_color())
}

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub fn code() -> Style {
    Style::default()
}

pub fn inline_code() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn bar() -> Style {
    Style::default()
}

/// Raised surface for the prompt composer.
pub fn composer() -> Style {
    composer_for(crossterm::style::available_color_count())
}

fn composer_for(color_count: u16) -> Style {
    match color_count {
        u16::MAX => Style::default()
            .fg(Color::Rgb(0xf2, 0xf2, 0xf2))
            .bg(Color::Rgb(0x32, 0x36, 0x3d)),
        colors if colors >= 256 => Style::default()
            .fg(Color::Indexed(255))
            .bg(Color::Indexed(238)),
        _ => Style::default().fg(Color::White).bg(Color::DarkGray),
    }
}

pub fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// The frame this indicator shows on an animation tick.
pub fn pulse(kind: Pulse, tick: usize) -> &'static str {
    let frames = kind.frames();
    frames[(tick / kind.every()) % frames.len()]
}

/// Human-readable elapsed time, sized to the magnitude.
pub fn duration(millis: u64) -> String {
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    if millis < 60_000 {
        return format!("{:.1}s", millis as f64 / 1000.0);
    }

    let seconds = millis / 1_000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    let weeks = days / 7;
    let seconds = seconds % 60;
    let minutes = minutes % 60;
    let hours = hours % 24;
    let days = days % 7;

    if weeks > 0 {
        format!("{weeks}w{days}d {hours}h{minutes:02}m{seconds:02}s")
    } else if days > 0 {
        format!("{days}d {hours}h{minutes:02}m{seconds:02}s")
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else {
        format!("{minutes}m{seconds:02}s")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Style};

    use super::{
        BRAND_ANSI, BRAND_RGB, bar, brand_rainbow_for, code, composer, composer_for, duration,
        selection, text,
    };

    #[test]
    fn formats_turn_durations_through_weeks() {
        assert_eq!(duration(0), "0ms");
        assert_eq!(duration(999), "999ms");
        assert_eq!(duration(1_000), "1.0s");
        assert_eq!(duration(60_000), "1m00s");
        assert_eq!(duration(3_600_000), "1h00m00s");
        assert_eq!(duration(86_400_000), "1d 0h00m00s");
        assert_eq!(duration(604_800_000), "1w0d 0h00m00s");
        assert_eq!(duration(788_645_000), "1w2d 3h04m05s");
    }

    #[test]
    fn functional_styles_inherit_the_terminal_background() {
        assert_eq!(text().fg, None);
        assert_eq!(text().bg, None);
        assert_eq!(code().bg, None);
        assert_eq!(bar().bg, None);
        assert!(!bar().add_modifier.contains(Modifier::DIM));
        assert_eq!(super::running_color(), Color::Cyan);
        assert!(selection().add_modifier.contains(Modifier::REVERSED));
        assert!(
            !super::inline_code()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn composer_uses_a_contrasting_surface() {
        assert!(composer().fg.is_some());
        assert!(composer().bg.is_some());
        assert_eq!(
            composer_for(u16::MAX),
            Style::default()
                .fg(Color::Rgb(0xf2, 0xf2, 0xf2))
                .bg(Color::Rgb(0x32, 0x36, 0x3d))
        );
        assert_eq!(composer_for(256).bg, Some(Color::Indexed(238)));
        assert_eq!(composer_for(8).bg, Some(Color::DarkGray));
    }

    #[test]
    fn true_colour_uses_the_speakeasy_brand_ramp() {
        assert_eq!(brand_rainbow_for(u16::MAX), &BRAND_RGB);
        assert_eq!(BRAND_RGB[0], Color::Rgb(0x32, 0x0f, 0x1e));
        assert_eq!(BRAND_RGB[8], Color::Rgb(0x9b, 0xc3, 0xff));
    }

    #[test]
    fn limited_colour_uses_the_terminal_palette() {
        assert_eq!(brand_rainbow_for(256), &BRAND_ANSI);
        assert_eq!(brand_rainbow_for(8), &BRAND_ANSI);
        assert!(
            BRAND_ANSI
                .iter()
                .all(|color| !matches!(color, Color::Rgb(..)))
        );
    }
}
