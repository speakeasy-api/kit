use std::io::{self, Write};

use crossterm::{
    cursor::{RestorePosition, SavePosition},
    queue,
};
use ratatui::{
    CompletedFrame,
    backend::{Backend, CrosstermBackend},
    buffer::{Cell, CellDiffOption, CellWidth},
    layout::Rect,
};

use super::wrap::LinkHit;

const OSC8_OPEN: &[u8] = b"\x1b]8;;";
const OSC8_CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

#[derive(Default)]
pub struct HyperlinkRenderer {
    linked: Vec<Footprint>,
}

#[derive(Clone, Copy)]
struct Footprint {
    y: u16,
    start: u16,
    end: u16,
}

pub struct PreparedFrame {
    stale: Vec<DrawCell>,
    links: Vec<PreparedLink>,
    footprints: Vec<Footprint>,
}

struct PreparedLink {
    url: String,
    cells: Vec<DrawCell>,
}

struct DrawCell {
    x: u16,
    y: u16,
    cell: Cell,
}

impl HyperlinkRenderer {
    /// Copies the bounded link rows out of Ratatui's completed frame so the
    /// terminal backend can be borrowed again for the native-link pass.
    pub fn prepare(
        &self,
        frame: &CompletedFrame<'_>,
        row_links: &[Vec<LinkHit>],
        transcript_left: usize,
        transcript_top: usize,
        obscured: bool,
    ) -> PreparedFrame {
        let stale = self
            .linked
            .iter()
            .flat_map(|footprint| cells(frame, *footprint))
            .collect();
        let mut links = Vec::new();
        let mut footprints = Vec::new();

        if !obscured {
            for (row, hits) in row_links.iter().enumerate() {
                let Some(y) = transcript_top
                    .checked_add(row)
                    .and_then(|value| u16::try_from(value).ok())
                else {
                    continue;
                };
                for hit in hits {
                    let Some(start) = transcript_left
                        .checked_add(hit.start)
                        .and_then(|value| u16::try_from(value).ok())
                    else {
                        continue;
                    };
                    let Some(end) = transcript_left
                        .checked_add(hit.end)
                        .and_then(|value| u16::try_from(value).ok())
                    else {
                        continue;
                    };
                    let Some(footprint) = clipped(frame.area, Footprint { y, start, end }) else {
                        continue;
                    };
                    let link_cells = cells(frame, footprint);
                    if link_cells.is_empty() {
                        continue;
                    }
                    links.push(PreparedLink {
                        url: escape_url(&hit.url),
                        cells: link_cells,
                    });
                    footprints.push(footprint);
                }
            }
        }

        PreparedFrame {
            stale,
            links,
            footprints,
        }
    }

    /// Repaints old footprints without a link, then repaints current footprints
    /// between OSC-8 open/close commands. Save/restore leaves Ratatui's cursor
    /// exactly where its normal draw pass placed it.
    pub fn draw<W: Write>(
        &mut self,
        backend: &mut CrosstermBackend<W>,
        prepared: PreparedFrame,
    ) -> io::Result<()> {
        if prepared.stale.is_empty() && prepared.links.is_empty() {
            self.linked = prepared.footprints;
            return Ok(());
        }

        queue!(backend, SavePosition)?;
        let result: io::Result<()> = (|| {
            backend.draw(prepared.stale.iter().map(draw_tuple))?;
            for link in &prepared.links {
                backend.write_all(OSC8_OPEN)?;
                backend.write_all(link.url.as_bytes())?;
                backend.write_all(b"\x1b\\")?;
                backend.draw(link.cells.iter().map(draw_tuple))?;
                backend.write_all(OSC8_CLOSE)?;
            }
            Ok(())
        })();

        // Do not let a partial write strand either a hyperlink or the cursor.
        let cleanup: io::Result<()> = (|| {
            backend.write_all(OSC8_CLOSE)?;
            queue!(backend, RestorePosition)?;
            Backend::flush(backend)
        })();
        result?;
        cleanup?;
        self.linked = prepared.footprints;
        Ok(())
    }
}

fn draw_tuple(cell: &DrawCell) -> (u16, u16, &Cell) {
    (cell.x, cell.y, &cell.cell)
}

fn cells(frame: &CompletedFrame<'_>, footprint: Footprint) -> Vec<DrawCell> {
    let Some(footprint) = clipped(frame.area, footprint) else {
        return Vec::new();
    };
    let mut cells = Vec::new();
    let mut covered_until = frame.area.left();
    for x in frame.area.left()..frame.area.right() {
        if x < covered_until {
            continue;
        }
        let Some(cell) = frame.buffer.cell((x, footprint.y)) else {
            continue;
        };
        let grapheme_end = x
            .saturating_add(cell.cell_width().max(1))
            .min(frame.area.right());
        covered_until = grapheme_end;
        if cell_is_skipped(cell) {
            continue;
        }
        if x < footprint.end && grapheme_end > footprint.start {
            cells.push(DrawCell {
                x,
                y: footprint.y,
                cell: cell.clone(),
            });
        }
    }
    cells
}

#[allow(deprecated)]
fn cell_is_skipped(cell: &Cell) -> bool {
    matches!(cell.diff_option, CellDiffOption::Skip) || cell.skip
}

fn clipped(area: Rect, footprint: Footprint) -> Option<Footprint> {
    if footprint.y < area.top() || footprint.y >= area.bottom() {
        return None;
    }
    let start = footprint.start.max(area.left());
    let end = footprint.end.min(area.right());
    (start < end).then_some(Footprint {
        y: footprint.y,
        start,
        end,
    })
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
    use std::{cell::RefCell, io::Write, rc::Rc};

    use ratatui::{
        CompletedFrame, Terminal, backend::CrosstermBackend, buffer::Buffer, layout::Rect,
        text::Line,
    };

    use super::*;

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

    impl Capture {
        fn bytes(&self) -> Vec<u8> {
            self.0.borrow().clone()
        }

        fn clear(&self) {
            self.0.borrow_mut().clear();
        }
    }

    fn has_nonempty_open(output: &[u8]) -> bool {
        output
            .windows(OSC8_OPEN.len())
            .enumerate()
            .any(|(index, prefix)| {
                prefix == OSC8_OPEN && !output[index + OSC8_OPEN.len()..].starts_with(b"\x1b\\")
            })
    }

    fn frame(buffer: &Buffer) -> CompletedFrame<'_> {
        CompletedFrame {
            buffer,
            area: buffer.area,
            count: 0,
        }
    }

    #[test]
    fn emits_native_open_and_close_around_linked_cells_and_preserves_cursor() {
        let buffer = Buffer::with_lines([Line::from("sent Image #1")]);
        let rows = vec![vec![LinkHit {
            start: 5,
            end: 13,
            url: "file:///tmp/image.png".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let prepared = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());

        renderer.draw(&mut backend, prepared).unwrap();

        let output = capture.bytes();
        let open = b"\x1b]8;;file:///tmp/image.png\x1b\\";
        let open_at = output
            .windows(open.len())
            .position(|part| part == open)
            .unwrap();
        let text_at = output
            .windows(8)
            .position(|part| part == b"Image #1")
            .unwrap();
        let close_at = output
            .windows(OSC8_CLOSE.len())
            .position(|part| part == OSC8_CLOSE)
            .unwrap();
        assert!(open_at < text_at && text_at < close_at);
        assert!(output.starts_with(b"\x1b7"));
        assert!(output.ends_with(b"\x1b8"));
    }

    #[test]
    fn redraws_stale_footprints_without_reopening_a_link() {
        let buffer = Buffer::with_lines([Line::from("Image #1")]);
        let rows = vec![vec![LinkHit {
            start: 0,
            end: 8,
            url: "file:///tmp/image.png".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());
        let linked = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        renderer.draw(&mut backend, linked).unwrap();
        capture.clear();

        let cleared = renderer.prepare(&frame(&buffer), &[], 0, 0, false);
        renderer.draw(&mut backend, cleared).unwrap();

        let output = capture.bytes();
        assert!(output.windows(8).any(|part| part == b"Image #1"));
        assert!(!has_nonempty_open(&output));
        assert!(output.starts_with(b"\x1b7"));
        assert!(output.ends_with(b"\x1b8"));
    }

    #[test]
    fn obscured_frames_clear_existing_native_links() {
        let buffer = Buffer::with_lines([Line::from("Image #1")]);
        let rows = vec![vec![LinkHit {
            start: 0,
            end: 8,
            url: "file:///tmp/image.png".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());
        let linked = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        renderer.draw(&mut backend, linked).unwrap();
        capture.clear();

        let obscured = renderer.prepare(&frame(&buffer), &rows, 0, 0, true);
        renderer.draw(&mut backend, obscured).unwrap();

        assert!(!has_nonempty_open(&capture.bytes()));
    }

    #[test]
    fn control_characters_cannot_terminate_the_osc_sequence() {
        assert_eq!(
            escape_url("file:///tmp/a\x1b]8;;evil\x07\u{85}"),
            "file:///tmp/a%1B]8;;evil%07%C2%85"
        );
    }

    #[test]
    fn emits_each_wide_grapheme_once() {
        let buffer = Buffer::with_lines([Line::from("A界🙂B")]);
        let rows = vec![vec![LinkHit {
            start: 1,
            end: 5,
            url: "https://example.com/wide".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let prepared = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());

        renderer.draw(&mut backend, prepared).unwrap();

        let output = capture.bytes();
        assert_eq!(
            output
                .windows("界".len())
                .filter(|part| *part == "界".as_bytes())
                .count(),
            1
        );
        assert_eq!(
            output
                .windows("🙂".len())
                .filter(|part| *part == "🙂".as_bytes())
                .count(),
            1
        );
        assert!(
            !output
                .windows("界 ".len())
                .any(|part| part == "界 ".as_bytes())
        );
        assert!(
            !output
                .windows("🙂 ".len())
                .any(|part| part == "🙂 ".as_bytes())
        );
    }

    #[test]
    fn stale_range_inside_a_wide_cell_repaints_the_whole_grapheme() {
        let buffer = Buffer::with_lines([Line::from("A界B")]);
        let rows = vec![vec![LinkHit {
            start: 2,
            end: 3,
            url: "https://example.com/wide".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());
        let linked = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        renderer.draw(&mut backend, linked).unwrap();
        capture.clear();

        let cleared = renderer.prepare(&frame(&buffer), &[], 0, 0, false);
        renderer.draw(&mut backend, cleared).unwrap();

        let output = capture.bytes();
        assert!(
            output
                .windows("界".len())
                .any(|part| part == "界".as_bytes())
        );
        assert!(!has_nonempty_open(&output));
    }

    #[test]
    fn omits_cells_marked_to_skip() {
        let mut buffer = Buffer::with_lines([Line::from("AB")]);
        buffer
            .cell_mut((0, 0))
            .unwrap()
            .set_diff_option(CellDiffOption::Skip);
        let rows = vec![vec![LinkHit {
            start: 0,
            end: 2,
            url: "https://example.com/skip".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let prepared = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());

        renderer.draw(&mut backend, prepared).unwrap();

        let output = capture.bytes();
        assert!(!output.contains(&b'A'));
        assert!(output.contains(&b'B'));
    }

    #[test]
    fn renders_from_a_real_completed_terminal_frame() {
        let capture = Capture::default();
        let backend = CrosstermBackend::new(capture.clone());
        let mut terminal = Terminal::new(backend).unwrap();
        let completed = terminal
            .draw(|frame| {
                frame.render_widget(Line::from("Image #1"), Rect::new(0, 0, 8, 1));
            })
            .unwrap();
        let rows = vec![vec![LinkHit {
            start: 0,
            end: 8,
            url: "file:///tmp/image.png".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let prepared = renderer.prepare(&completed, &rows, 0, 0, false);
        capture.clear();

        renderer.draw(terminal.backend_mut(), prepared).unwrap();

        let output = capture.bytes();
        assert!(has_nonempty_open(&output));
        assert!(output.windows(8).any(|part| part == b"Image #1"));
    }

    struct RejectWrites;

    impl Write for RejectWrites {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("rejected write"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn propagates_backend_write_errors() {
        let buffer = Buffer::with_lines([Line::from("Image #1")]);
        let rows = vec![vec![LinkHit {
            start: 0,
            end: 8,
            url: "file:///tmp/image.png".into(),
        }]];
        let mut renderer = HyperlinkRenderer::default();
        let prepared = renderer.prepare(&frame(&buffer), &rows, 0, 0, false);
        let mut backend = CrosstermBackend::new(RejectWrites);

        let error = renderer.draw(&mut backend, prepared).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
}
