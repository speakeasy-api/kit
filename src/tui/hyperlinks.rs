//! OSC-8 metadata travels beside Ratatui's buffer, not inside display symbols.
//! Metadata-only changes join the diff; the backend decorates the resulting draw
//! stream in runs, so every changed grapheme is printed once.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{self, Write},
    rc::Rc,
};

use super::wrap::LinkHit;
use crossterm::{
    cursor::{Hide, RestorePosition, SavePosition, Show},
    queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::{Buffer, Cell, CellDiffOption, CellWidth},
    layout::{Position, Size},
};

type Links = BTreeMap<(u16, u16), Rc<str>>;
const OSC8_OPEN: &[u8] = b"\x1b]8;;";
const OSC8_CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

#[derive(Default)]
struct Metadata {
    links: Links,
    forced: Vec<(u16, u16, Cell)>,
}

pub struct HyperlinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
    metadata: Rc<RefCell<Metadata>>,
    cursor_visible: bool,
    in_frame: bool,
    saved_position: bool,
}

impl<W: Write> HyperlinkBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            metadata: Rc::default(),
            cursor_visible: true,
            in_frame: false,
            saved_position: false,
        }
    }

    fn begin_frame(&mut self) -> io::Result<()> {
        self.in_frame = true;
        self.saved_position = false;
        queue!(self.inner, BeginSynchronizedUpdate, SavePosition)?;
        self.saved_position = true;
        queue!(self.inner, Hide)
    }

    // Attempt every cleanup independently: a failing OSC close must not prevent
    // ending synchronized updates or restoring the cursor after an I/O failure.
    fn end_frame(&mut self, failed: bool, was_visible: bool) -> io::Result<()> {
        let close = self.inner.write_all(OSC8_CLOSE);
        let restore = if failed && self.saved_position {
            queue!(self.inner, RestorePosition)
        } else {
            Ok(())
        };
        if failed {
            self.cursor_visible = was_visible;
        }
        let cursor = if self.cursor_visible {
            queue!(self.inner, Show)
        } else {
            queue!(self.inner, Hide)
        };
        let end = queue!(self.inner, EndSynchronizedUpdate);
        let flush = Backend::flush(&mut self.inner);
        self.in_frame = false;
        close.and(restore).and(cursor).and(end).and(flush)
    }
}

pub struct FrameLinks {
    pub rows: Vec<Vec<LinkHit>>,
    pub left: usize,
    pub top: usize,
    pub obscured: bool,
}

/// Enclose *all* terminal output, including autoresize and cursor updates.
/// Cursor visibility requests are deferred until after Ratatui positions it.
pub fn draw<W: Write>(
    terminal: &mut Terminal<HyperlinkBackend<W>>,
    render: impl FnOnce(&mut Frame<'_>) -> FrameLinks,
) -> io::Result<()> {
    let metadata = Rc::clone(&terminal.backend().metadata);
    let was_visible = terminal.backend().cursor_visible;
    let result = terminal.backend_mut().begin_frame().and_then(|()| {
        terminal
            .draw(|frame| {
                let rendered = render(frame);
                let next = prepare(
                    frame.buffer_mut(),
                    &rendered.rows,
                    rendered.left,
                    rendered.top,
                    rendered.obscured,
                );
                let mut previous = metadata.borrow_mut();
                previous.forced = invalidate(frame.buffer_mut(), &previous.links, &next);
                previous.links = next;
            })
            .map(|_| ())
    });
    let cleanup = terminal
        .backend_mut()
        .end_frame(result.is_err(), was_visible);
    result.and(cleanup)
}

fn prepare(
    buffer: &Buffer,
    rows: &[Vec<LinkHit>],
    left: usize,
    top: usize,
    obscured: bool,
) -> Links {
    let mut links = Links::new();
    if obscured {
        return links;
    }
    for (row, hits) in rows.iter().enumerate() {
        let Some(y) = top.checked_add(row).and_then(|y| u16::try_from(y).ok()) else {
            continue;
        };
        if y < buffer.area.top() || y >= buffer.area.bottom() {
            continue;
        }
        if hits.is_empty() {
            continue;
        }
        let hits: Vec<_> = hits
            .iter()
            // Internal on-demand targets require a normal TUI click;
            // terminal modifier-click must not dispatch them to the OS.
            .filter(|hit| !hit.url.starts_with("kit-image:"))
            .map(|hit| {
                (
                    left.saturating_add(hit.start),
                    left.saturating_add(hit.end),
                    Rc::<str>::from(escape_url(&hit.url)),
                )
            })
            .collect();
        let mut covered_until = buffer.area.left();
        for x in buffer.area.left()..buffer.area.right() {
            if x < covered_until {
                continue;
            }
            let cell = &buffer[(x, y)];
            covered_until = x.saturating_add(cell.cell_width().max(1));
            if cell_is_skipped(cell) {
                continue;
            }
            // Match any overlap, including a hit beginning in a wide grapheme.
            if let Some((_, _, url)) = hits.iter().rev().find(|(start, end, _)| {
                start < end && usize::from(x) < *end && usize::from(covered_until) > *start
            }) {
                links.insert((x, y), Rc::clone(url));
            }
        }
    }
    links
}

fn invalidate(buffer: &Buffer, old: &Links, new: &Links) -> Vec<(u16, u16, Cell)> {
    let mut forced = Vec::new();
    if old == new {
        return forced;
    }
    // Walk grapheme starts, never the continuation cell or an image's Skip cell.
    // A former link may now lie inside a *different* wide grapheme.
    for y in buffer.area.top()..buffer.area.bottom() {
        let mut x = buffer.area.left();
        while x < buffer.area.right() {
            let cell = &buffer[(x, y)];
            let end = x
                .saturating_add(cell.cell_width().max(1))
                .min(buffer.area.right());
            if !cell_is_skipped(cell)
                && (x..end).any(|column| old.get(&(column, y)) != new.get(&(column, y)))
            {
                forced.push((x, y, cell.clone()));
            }
            x = end;
        }
    }
    forced
}

#[allow(deprecated)]
fn cell_is_skipped(cell: &Cell) -> bool {
    matches!(cell.diff_option, CellDiffOption::Skip) || cell.skip
}

impl<W: Write> Write for HyperlinkBackend<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for HyperlinkBackend<W> {
    type Error = io::Error;
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut metadata = self.metadata.borrow_mut();
        let forced = std::mem::take(&mut metadata.forced);
        let links = &metadata.links;
        // Merge metadata-only updates with the real diff before printing. Using
        // AlwaysUpdate on the frame would persist in Ratatui's previous buffer
        // and cause a redundant repaint when that marker disappears next frame.
        let mut extra: BTreeMap<_, _> =
            forced.iter().map(|(x, y, cell)| ((*y, *x), cell)).collect();
        let updates: Vec<_> = content
            .inspect(|(x, y, _)| {
                extra.remove(&(*y, *x));
            })
            .collect();
        // Preserve Ratatui's order: VS16 wide-grapheme updates can clear
        // continuation cells *before* printing their leading grapheme.
        let mut content = extra
            .into_iter()
            .map(|((y, x), cell)| (x, y, cell))
            .chain(updates)
            .peekable();
        while let Some(&(x, y, _)) = content.peek() {
            let target = links.get(&(x, y));
            if let Some(url) = target {
                self.inner.write_all(OSC8_OPEN)?;
                self.inner.write_all(url.as_bytes())?;
                self.inner.write_all(b"\x1b\\")?;
            }
            let run = std::iter::from_fn(|| {
                let &(x, y, _) = content.peek()?;
                if links.get(&(x, y)) == target {
                    content.next()
                } else {
                    None
                }
            });
            let result = self.inner.draw(run);
            let close = if target.is_some() {
                self.inner.write_all(OSC8_CLOSE)
            } else {
                Ok(())
            };
            result.and(close)?;
        }
        Ok(())
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.cursor_visible = false;
        if self.in_frame {
            Ok(())
        } else {
            self.inner.hide_cursor()
        }
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.cursor_visible = true;
        if self.in_frame {
            Ok(())
        } else {
            self.inner.show_cursor()
        }
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
        self.inner.set_cursor_position(p)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.metadata.borrow_mut().links.clear();
        self.inner.clear()
    }
    fn clear_region(&mut self, kind: ClearType) -> io::Result<()> {
        self.inner.clear_region(kind)
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

fn escape_url(url: &str) -> String {
    let mut escaped = String::with_capacity(url.len());
    for character in url.chars() {
        if character.is_control() {
            use std::fmt::Write as _;
            for byte in character.to_string().bytes() {
                let _ = write!(escaped, "%{byte:02X}");
            }
        } else {
            escaped.push(character);
        }
    }
    escaped
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;
    use ratatui::{TerminalOptions, Viewport, layout::Rect, text::Line};

    #[derive(Clone, Default)]
    struct Capture(Rc<RefCell<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn terminal() -> (Terminal<HyperlinkBackend<Capture>>, Capture) {
        let capture = Capture::default();
        let terminal = Terminal::with_options(
            HyperlinkBackend::new(capture.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 12, 2)),
            },
        )
        .unwrap();
        (terminal, capture)
    }
    fn render(
        terminal: &mut Terminal<HyperlinkBackend<Capture>>,
        text: &str,
        url: Option<&str>,
        cursor: bool,
    ) {
        draw(terminal, |frame| {
            frame.render_widget(Line::from(text), Rect::new(0, 0, 12, 1));
            if cursor {
                frame.set_cursor_position((10, 1));
            }
            FrameLinks {
                rows: vec![
                    url.map(|url| LinkHit {
                        start: 0,
                        end: 4,
                        url: url.into(),
                    })
                    .into_iter()
                    .collect(),
                ],
                left: 0,
                top: 0,
                obscured: false,
            }
        })
        .unwrap();
    }
    fn output(capture: &Capture) -> String {
        String::from_utf8(std::mem::take(&mut *capture.0.borrow_mut())).unwrap()
    }
    #[test]
    fn internal_image_targets_are_not_emitted_as_native_hyperlinks() {
        let (mut terminal, capture) = terminal();
        render(&mut terminal, "LINK", Some("kit-image:internal"), true);
        let internal = output(&capture);
        assert!(internal.contains("LINK"));
        assert!(!internal.contains("kit-image:"));
        assert_eq!(
            internal.matches("\x1b]8;;").count(),
            internal.matches("\x1b]8;;\x1b\\").count()
        );

        // Replacing a native target with an internal target also clears the
        // old hyperlink, even when its displayed text is unchanged.
        render(&mut terminal, "LINK", Some("https://example.com"), true);
        assert!(output(&capture).contains("https://example.com"));
        render(&mut terminal, "LINK", Some("kit-image:internal"), true);
        let cleared = output(&capture);
        assert!(cleared.contains("LINK"));
        assert!(!cleared.contains("kit-image:"));
        assert!(!cleared.contains("https://example.com"));
    }

    #[test]
    fn one_pass_and_metadata_only_changes() {
        let (mut terminal, capture) = terminal();
        render(&mut terminal, "LINK", Some("https://one"), true);
        let first = output(&capture);
        assert_eq!(first.matches("LINK").count(), 1);
        assert!(first.contains("\x1b]8;;https://one\x1b\\"));
        render(&mut terminal, "LINK", Some("https://one"), true);
        let unchanged = output(&capture);
        assert!(!unchanged.contains("LINK"));
        assert!(!unchanged.contains("https://one"));
        render(&mut terminal, "LINK", Some("https://two"), true);
        let changed = output(&capture);
        assert_eq!(changed.matches("LINK").count(), 1);
        assert!(changed.contains("https://two"));
        assert!(!changed.contains("https://one"));
        render(&mut terminal, "LINK", None, true);
        let stale = output(&capture);
        assert_eq!(stale.matches("LINK").count(), 1);
        assert!(!stale.contains("https://"));
        render(&mut terminal, "LINK", None, true);
        assert!(!output(&capture).contains("LINK"));
    }
    #[test]
    fn ordinary_frames_hide_before_print_and_show_after_positioning() {
        let (mut terminal, capture) = terminal();
        render(&mut terminal, "TEXT", None, true);
        let visible = output(&capture);
        assert!(visible.starts_with("\x1b[?2026h\x1b7\x1b[?25l"));
        assert!(visible.find("\x1b[?25l").unwrap() < visible.find("TEXT").unwrap());
        assert!(visible.find("\x1b[2;11H").unwrap() < visible.find("\x1b[?25h").unwrap());
        assert!(visible.ends_with("\x1b[?25h\x1b[?2026l"));
        render(&mut terminal, "TEXT", None, false);
        let hidden = output(&capture);
        assert!(!hidden.contains("\x1b[?25h"));
        assert!(hidden.ends_with("\x1b[?25l\x1b[?2026l"));
    }
    #[test]
    fn wide_graphemes_skipped_cells_and_obscured_links() {
        let (mut terminal, capture) = terminal();
        let mut obscured = false;
        for _ in 0..2 {
            draw(&mut terminal, |frame| {
                frame.render_widget(Line::from("A界🙂Z"), Rect::new(0, 0, 12, 1));
                frame.buffer_mut()[(5, 0)].set_diff_option(CellDiffOption::Skip);
                FrameLinks {
                    rows: vec![vec![LinkHit {
                        start: 2,
                        end: 6,
                        url: "https://wide".into(),
                    }]],
                    left: 0,
                    top: 0,
                    obscured,
                }
            })
            .unwrap();
            let text = output(&capture);
            assert_eq!(text.matches('界').count(), 1);
            assert_eq!(text.matches('🙂').count(), 1);
            assert!(!text.contains('Z'));
            assert_eq!(text.contains("https://wide"), !obscured);
            obscured = true;
        }
    }
    #[test]
    fn escapes_control_characters() {
        assert_eq!(
            escape_url("https://a/\x1b\\\n\u{9c}"),
            "https://a/%1B\\%0A%C2%9C"
        );
    }
    #[test]
    fn scrolling_same_text_moves_link_metadata_without_double_printing() {
        let (mut terminal, capture) = terminal();
        for top in [0, 1] {
            draw(&mut terminal, |frame| {
                for y in 0..2 {
                    frame.render_widget(Line::from("LINK"), Rect::new(0, y, 12, 1));
                }
                FrameLinks {
                    rows: vec![vec![LinkHit {
                        start: 0,
                        end: 4,
                        url: "https://moving".into(),
                    }]],
                    left: 0,
                    top,
                    obscured: false,
                }
            })
            .unwrap();
            let text = output(&capture);
            assert_eq!(text.matches("LINK").count(), 2);
            assert_eq!(text.matches("https://moving").count(), 1);
        }
    }

    #[test]
    fn preserves_ratatui_wide_cell_clear_order() {
        let old = Buffer::with_lines(["ABCD"]);
        let next = Buffer::with_lines(["❤️CD"]);
        let capture = Capture::default();
        let plain = Capture::default();
        let mut backend = HyperlinkBackend::new(capture.clone());
        backend.draw(old.diff_iter(&next)).unwrap();
        CrosstermBackend::new(plain.clone())
            .draw(old.diff_iter(&next))
            .unwrap();
        assert_eq!(output(&capture), output(&plain));
    }

    // External I/O boundary: fail a particular output command once, then allow
    // cleanup through. No production instrumentation is needed.
    struct FailCommand {
        capture: Capture,
        command: &'static [u8],
        failed: bool,
    }
    impl Write for FailCommand {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.failed && bytes == self.command {
                self.failed = true;
                return Err(io::Error::other("injected write failure"));
            }
            self.capture.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failures_close_links_end_sync_and_restore_cursor_visibility() {
        for visible in [false, true] {
            let capture = Capture::default();
            let writer = FailCommand {
                capture: capture.clone(),
                command: b"L",
                failed: false,
            };
            let mut terminal = Terminal::with_options(
                HyperlinkBackend::new(writer),
                TerminalOptions {
                    viewport: Viewport::Fixed(Rect::new(0, 0, 12, 2)),
                },
            )
            .unwrap();
            if !visible {
                terminal.hide_cursor().unwrap();
            }
            output(&capture);
            let result = draw(&mut terminal, |frame| {
                frame.render_widget(Line::from("LINK"), Rect::new(0, 0, 12, 1));
                FrameLinks {
                    rows: vec![vec![LinkHit {
                        start: 0,
                        end: 4,
                        url: "https://fail".into(),
                    }]],
                    left: 0,
                    top: 0,
                    obscured: false,
                }
            });
            assert!(result.is_err());
            let text = output(&capture);
            assert!(text.contains("\x1b]8;;\x1b\\"));
            assert!(text.contains("\x1b8"));
            let tail = if visible {
                "\x1b[?25h\x1b[?2026l"
            } else {
                "\x1b[?25l\x1b[?2026l"
            };
            assert!(text.ends_with(tail), "{text:?}");
        }
    }

    #[test]
    fn cleanup_failure_does_not_skip_cursor_or_end_sync() {
        let capture = Capture::default();
        let mut backend = HyperlinkBackend::new(FailCommand {
            capture: capture.clone(),
            command: OSC8_CLOSE,
            failed: false,
        });
        backend.begin_frame().unwrap();
        assert!(backend.end_frame(true, true).is_err());
        let text = output(&capture);
        assert!(text.ends_with("\x1b8\x1b[?25h\x1b[?2026l"));
    }
    #[test]
    fn failed_begin_does_not_restore_an_unsaved_cursor_position() {
        let capture = Capture::default();
        let writer = FailCommand {
            capture: capture.clone(),
            command: b"\x1b[?2026h",
            failed: false,
        };
        let mut terminal = Terminal::with_options(
            HyperlinkBackend::new(writer),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 12, 2)),
            },
        )
        .unwrap();
        let result = draw(&mut terminal, |_| {
            panic!("must not render after failed begin")
        });
        assert!(result.is_err());
        let text = output(&capture);
        assert!(!text.contains("\x1b8"));
        assert!(text.ends_with("\x1b[?25h\x1b[?2026l"));
    }
}
