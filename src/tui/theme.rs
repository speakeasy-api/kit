//! Colours and glyphs for the terminal client.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(94, 205, 190);
pub const USER: Color = Color::Rgb(137, 180, 250);
pub const RUNNING: Color = Color::Rgb(197, 168, 250);
pub const SUCCESS: Color = Color::Rgb(126, 200, 130);
pub const WARN: Color = Color::Rgb(230, 180, 100);
pub const ERROR: Color = Color::Rgb(240, 120, 120);
pub const DIM: Color = Color::Rgb(122, 130, 145);
pub const FAINT: Color = Color::Rgb(78, 85, 98);
pub const TEXT: Color = Color::Rgb(215, 220, 228);
pub const CODE_BG: Color = Color::Rgb(30, 33, 41);
pub const BAR_BG: Color = Color::Rgb(24, 27, 34);

/// Frames of the activity spinner, one per animation tick.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn text() -> Style {
    Style::default().fg(TEXT)
}

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub fn code() -> Style {
    Style::default().fg(Color::Rgb(232, 200, 140)).bg(CODE_BG)
}

pub fn bar() -> Style {
    Style::default().fg(DIM).bg(BAR_BG)
}

/// The spinner frame for an animation tick.
pub fn spinner(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
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
