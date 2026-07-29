use std::{
    fmt,
    io::{self, Write},
};

#[derive(Clone, Copy)]
pub(crate) struct FileSide {
    pub mode: u32,
    pub binary: bool,
    pub lines: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum FileDiff {
    ExactMove,
    Binary,
    Text,
}

#[derive(Clone, Copy)]
pub(crate) enum DiffSide {
    Before,
    After,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_file_header(
    output: &mut impl Write,
    from: &str,
    to: &str,
    before: Option<FileSide>,
    after: Option<FileSide>,
    moved: bool,
    same_content: bool,
) -> io::Result<FileDiff> {
    writeln!(output, "diff --git a/{from} b/{to}")?;
    let exact_move = moved
        && same_content
        && matches!((before, after), (Some(before), Some(after)) if before.mode == after.mode);
    if exact_move {
        writeln!(output, "similarity index 100%")?;
    }
    if moved {
        writeln!(output, "rename from {from}")?;
        writeln!(output, "rename to {to}")?;
    }
    match (before, after) {
        (None, Some(after)) => writeln!(output, "new file mode {:06o}", after.mode)?,
        (Some(before), None) => writeln!(output, "deleted file mode {:06o}", before.mode)?,
        (Some(before), Some(after)) if before.mode != after.mode => {
            writeln!(output, "old mode {:06o}", before.mode)?;
            writeln!(output, "new mode {:06o}", after.mode)?;
        }
        _ => {}
    }
    if exact_move {
        return Ok(FileDiff::ExactMove);
    }
    if before.is_some_and(|side| side.binary) || after.is_some_and(|side| side.binary) {
        writeln!(
            output,
            "Binary files {} and {} differ",
            before.map_or("/dev/null".to_owned(), |_| format!("a/{from}")),
            after.map_or("/dev/null".to_owned(), |_| format!("b/{to}")),
        )?;
        return Ok(FileDiff::Binary);
    }
    writeln!(
        output,
        "--- {}",
        before.map_or("/dev/null".to_owned(), |_| format!("a/{from}"))
    )?;
    writeln!(
        output,
        "+++ {}",
        after.map_or("/dev/null".to_owned(), |_| format!("b/{to}"))
    )?;
    writeln!(
        output,
        "@@ -{} +{} @@",
        hunk_range(before.map_or(0, |side| side.lines)),
        hunk_range(after.map_or(0, |side| side.lines)),
    )?;
    Ok(FileDiff::Text)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_file_diff<W, E, F>(
    output: &mut W,
    from: &str,
    to: &str,
    before: Option<FileSide>,
    after: Option<FileSide>,
    moved: bool,
    same_content: bool,
    mut content: F,
) -> Result<FileDiff, E>
where
    W: Write,
    E: From<io::Error>,
    F: FnMut(DiffSide, &mut PrefixedLines<'_, W>) -> Result<(), E>,
{
    let disposition =
        write_file_header(output, from, to, before, after, moved, same_content).map_err(E::from)?;
    if matches!(disposition, FileDiff::Text) {
        if before.is_some() {
            let mut lines = PrefixedLines::new(output, b'-');
            content(DiffSide::Before, &mut lines)?;
            lines.finish().map_err(E::from)?;
        }
        if after.is_some() {
            let mut lines = PrefixedLines::new(output, b'+');
            content(DiffSide::After, &mut lines)?;
            lines.finish().map_err(E::from)?;
        }
    }
    Ok(disposition)
}

pub(crate) struct PrefixedLines<'a, W> {
    output: &'a mut W,
    marker: u8,
    line_start: bool,
}

impl<'a, W: Write> PrefixedLines<'a, W> {
    pub(crate) fn new(output: &'a mut W, marker: u8) -> Self {
        Self {
            output,
            marker,
            line_start: true,
        }
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        if !self.line_start {
            self.output.write_all(b"\n\\ No newline at end of file\n")?;
        }
        Ok(())
    }
}

impl<W: Write> Write for PrefixedLines<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if self.line_start {
                self.output.write_all(&[self.marker])?;
                self.line_start = false;
            }
            if *byte == b'\n' {
                self.output.write_all(&bytes[start..=index])?;
                start = index + 1;
                self.line_start = true;
            }
        }
        if start < bytes.len() {
            self.output.write_all(&bytes[start..])?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[derive(Debug)]
pub enum ChangeDiffError {
    Limit,
    Io(io::Error),
}

impl fmt::Display for ChangeDiffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit => formatter.write_str("change diff exceeds its byte bound"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ChangeDiffError {}

impl From<io::Error> for ChangeDiffError {
    fn from(error: io::Error) -> Self {
        map_write_error(error)
    }
}

pub fn render_whole_file_replacement(
    path: &str,
    before: &[u8],
    after: &[u8],
    mode: u32,
    max_bytes: usize,
) -> Result<Vec<u8>, ChangeDiffError> {
    let side = |bytes: &[u8]| FileSide {
        mode,
        binary: bytes.contains(&0) || std::str::from_utf8(bytes).is_err(),
        lines: line_count(bytes),
    };
    let mut output = BoundedVec::new(max_bytes);
    write_file_diff::<_, ChangeDiffError, _>(
        &mut output,
        path,
        path,
        Some(side(before)),
        Some(side(after)),
        false,
        false,
        |side, lines| {
            lines.write_all(match side {
                DiffSide::Before => before,
                DiffSide::After => after,
            })?;
            Ok(())
        },
    )?;
    output.finish()
}

pub(crate) fn line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(bytes.last().is_some_and(|byte| *byte != b'\n'))
}

fn hunk_range(lines: usize) -> String {
    match lines {
        0 => "0,0".to_owned(),
        1 => "1".to_owned(),
        count => format!("1,{count}"),
    }
}

struct BoundedVec {
    bytes: Vec<u8>,
    max: usize,
    exceeded: bool,
}

impl BoundedVec {
    fn new(max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max,
            exceeded: false,
        }
    }

    fn finish(self) -> Result<Vec<u8>, ChangeDiffError> {
        if self.exceeded {
            Err(ChangeDiffError::Limit)
        } else {
            Ok(self.bytes)
        }
    }
}

impl Write for BoundedVec {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|next| next > self.max)
        {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "change diff exceeds its byte bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn map_write_error(error: io::Error) -> ChangeDiffError {
    if error.kind() == io::ErrorKind::FileTooLarge {
        ChangeDiffError::Limit
    } else {
        ChangeDiffError::Io(error)
    }
}
