use crate::workspace::edit::ir::{
    ByteRange, EditIr, EditOperation, Newline, RootRelativePath, TextContent,
};

use super::{NormalizationContext, NormalizeError, executable_mode};

#[derive(Clone, Debug)]
struct PatchLine<'a> {
    kind: LineKind,
    text: &'a str,
    no_newline: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineKind {
    Context,
    Remove,
    Add,
}

#[derive(Clone, Debug)]
struct Hunk<'a> {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<PatchLine<'a>>,
}

#[derive(Debug)]
struct Section<'a> {
    old_path: Option<RootRelativePath>,
    new_path: Option<RootRelativePath>,
    old_executable: Option<bool>,
    new_executable: Option<bool>,
    rename: Option<(RootRelativePath, RootRelativePath)>,
    hunks: Vec<Hunk<'a>>,
}

pub(super) fn normalize(
    input: &[u8],
    context: &NormalizationContext,
) -> Result<EditIr, NormalizeError> {
    if input.contains(&0) {
        return Err(NormalizeError::BinaryPatch);
    }
    let text = std::str::from_utf8(input).map_err(|_| NormalizeError::BinaryPatch)?;
    let mut lines = Vec::new();
    lines
        .try_reserve_exact(text.bytes().filter(|byte| *byte == b'\n').count() + 1)
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    lines.extend(
        text.split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line)),
    );
    let sections = parse_sections(&lines, context)?;
    let operation_count = sections
        .iter()
        .map(|section| {
            if section.rename.is_some() || section.old_path.is_none() || section.new_path.is_none()
            {
                1
            } else {
                section.hunks.len().max(1)
            }
        })
        .try_fold(0_usize, |total, count| total.checked_add(count))
        .ok_or(crate::workspace::edit::ir::IrError::Allocation)?;
    if operation_count > context.limits().max_operations {
        return Err(crate::workspace::edit::ir::IrError::OperationLimit {
            actual: operation_count,
            limit: context.limits().max_operations,
        }
        .into());
    }
    let mut operations = Vec::new();
    operations
        .try_reserve_exact(operation_count)
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    let mut content_bytes = 0;
    for section in sections {
        lower_section(section, context, &mut operations, &mut content_bytes)?;
    }
    EditIr::new(
        context.expected_revision().clone(),
        operations,
        context.limits(),
    )
    .map_err(Into::into)
}

fn parse_sections<'a>(
    lines: &[&'a str],
    context: &NormalizationContext,
) -> Result<Vec<Section<'a>>, NormalizeError> {
    let mut sections = Vec::new();
    let mut index = 0;
    let mut operation_count = 0_usize;
    let mut decoded_content_bytes = 0_usize;
    while index < lines.len() {
        while index < lines.len() && lines[index].is_empty() {
            index += 1;
        }
        if index == lines.len() {
            break;
        }
        if is_binary_marker(lines[index]) {
            return Err(NormalizeError::BinaryPatch);
        }
        count_operations(&mut operation_count, 1, context)?;
        let mut rename_from = None;
        let mut rename_to = None;
        let mut old_executable = None;
        let mut new_executable = None;
        let mut git_paths = None;
        if lines[index].starts_with("diff --git ") {
            git_paths = Some(parse_git_paths(lines[index], index + 1, context)?);
            index += 1;
            while index < lines.len()
                && !lines[index].starts_with("--- ")
                && !lines[index].starts_with("diff --git ")
            {
                let line = lines[index];
                if is_binary_marker(line) {
                    return Err(NormalizeError::BinaryPatch);
                } else if let Some(path) = line.strip_prefix("rename from ") {
                    if rename_from.is_some() {
                        return malformed(index + 1, "duplicate rename source");
                    }
                    rename_from = Some(parse_path(path, false, context)?);
                } else if let Some(path) = line.strip_prefix("rename to ") {
                    if rename_to.is_some() {
                        return malformed(index + 1, "duplicate rename destination");
                    }
                    rename_to = Some(parse_path(path, false, context)?);
                } else if let Some(mode) = line.strip_prefix("old mode ") {
                    set_mode(&mut old_executable, parse_mode(mode, index + 1)?, index + 1)?;
                } else if let Some(mode) = line.strip_prefix("new mode ") {
                    set_mode(&mut new_executable, parse_mode(mode, index + 1)?, index + 1)?;
                } else if let Some(mode) = line.strip_prefix("new file mode ") {
                    set_mode(&mut new_executable, parse_mode(mode, index + 1)?, index + 1)?;
                } else if let Some(mode) = line.strip_prefix("deleted file mode ") {
                    set_mode(&mut old_executable, parse_mode(mode, index + 1)?, index + 1)?;
                } else if line.starts_with("index ")
                    || line.starts_with("similarity index ")
                    || line.is_empty()
                {
                } else {
                    return malformed(index + 1, "unknown git patch metadata");
                }
                index += 1;
            }
        }
        let rename = match (rename_from, rename_to) {
            (Some(from), Some(to)) => Some((from, to)),
            (None, None) => None,
            _ => return malformed(index + 1, "incomplete rename metadata"),
        };
        if index >= lines.len() || !lines[index].starts_with("--- ") {
            if let Some(rename) = rename {
                if old_executable.is_some() != new_executable.is_some() {
                    return malformed(index + 1, "incomplete rename mode metadata");
                }
                check_git_paths(
                    git_paths.as_ref(),
                    Some(&rename.0),
                    Some(&rename.1),
                    index + 1,
                )?;
                sections
                    .try_reserve(1)
                    .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
                sections.push(Section {
                    old_path: None,
                    new_path: None,
                    old_executable,
                    new_executable,
                    rename: Some(rename),
                    hunks: Vec::new(),
                });
                continue;
            }
            if let Some((old_path, new_path)) = git_paths
                && old_executable.is_some()
                && new_executable.is_some()
            {
                if old_path != new_path {
                    return malformed(index + 1, "mode-only section changes path");
                }
                if old_executable == new_executable {
                    return malformed(index + 1, "mode-only section does not change mode");
                }
                sections
                    .try_reserve(1)
                    .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
                sections.push(Section {
                    old_path: Some(old_path),
                    new_path: Some(new_path),
                    old_executable,
                    new_executable,
                    rename: None,
                    hunks: Vec::new(),
                });
                continue;
            }
            return malformed(index + 1, "expected --- file header");
        }
        let old_path = parse_header_path(lines[index], "--- ", true, context)?;
        index += 1;
        if index >= lines.len() || !lines[index].starts_with("+++ ") {
            return malformed(index + 1, "expected +++ file header");
        }
        let new_path = parse_header_path(lines[index], "+++ ", true, context)?;
        index += 1;
        match (&old_path, &new_path) {
            (Some(_), Some(_)) if old_executable.is_some() != new_executable.is_some() => {
                return malformed(index, "incomplete mode change metadata");
            }
            (None, Some(_)) if old_executable.is_some() => {
                return malformed(index, "new-file section has old mode metadata");
            }
            (Some(_), None) if new_executable.is_some() => {
                return malformed(index, "deleted-file section has new mode metadata");
            }
            _ => {}
        }
        let logical_old = old_path.as_ref().or(new_path.as_ref());
        let logical_new = new_path.as_ref().or(old_path.as_ref());
        check_git_paths(git_paths.as_ref(), logical_old, logical_new, index)?;
        if let Some((from, to)) = rename.as_ref()
            && (logical_old != Some(from) || logical_new != Some(to))
        {
            return malformed(index, "rename and file-header paths disagree");
        }
        let mut hunks = Vec::new();
        while index < lines.len() && lines[index].starts_with("@@ ") {
            if old_path.is_some() && new_path.is_some() && rename.is_none() && !hunks.is_empty() {
                count_operations(&mut operation_count, 1, context)?;
            }
            let (hunk, next) = parse_hunk(
                lines,
                index,
                context.limits().max_content_bytes,
                &mut decoded_content_bytes,
            )?;
            hunks
                .try_reserve(1)
                .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
            hunks.push(hunk);
            index = next;
        }
        validate_hunk_coordinates(&hunks, index + 1)?;
        if rename.is_some() && !hunks.is_empty() {
            return Err(NormalizeError::UnsupportedPatch(
                "rename sections with content hunks are ambiguous".to_owned(),
            ));
        }
        if hunks.is_empty() && rename.is_none() && old_executable == new_executable {
            return malformed(index + 1, "file section has no hunks or mode change");
        }
        sections
            .try_reserve(1)
            .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
        sections.push(Section {
            old_path,
            new_path,
            old_executable,
            new_executable,
            rename,
            hunks,
        });
    }
    if sections.is_empty() {
        return malformed(1, "patch contains no file sections");
    }
    Ok(sections)
}

fn parse_hunk<'a>(
    lines: &[&'a str],
    start: usize,
    max_content_bytes: usize,
    decoded_content_bytes: &mut usize,
) -> Result<(Hunk<'a>, usize), NormalizeError> {
    let header = lines[start];
    let end = header[3..]
        .find(" @@")
        .map(|offset| offset + 3)
        .ok_or_else(|| NormalizeError::MalformedPatch {
            line: start + 1,
            reason: "invalid hunk header".to_owned(),
        })?;
    let mut ranges = header[3..end].split_whitespace();
    let (old_start, old_count) = parse_range(
        ranges.next().filter(|range| range.starts_with('-')),
        start + 1,
    )?;
    let (new_start, new_count) = parse_range(
        ranges.next().filter(|range| range.starts_with('+')),
        start + 1,
    )?;
    if ranges.next().is_some() {
        return malformed(start + 1, "invalid hunk range list");
    }
    let mut index = start + 1;
    let mut body = Vec::<PatchLine<'a>>::new();
    let mut actual_old = 0;
    let mut actual_new = 0;
    while actual_old < old_count || actual_new < new_count {
        let Some(&line) = lines.get(index) else {
            return malformed(start + 1, "hunk line counts do not match header");
        };
        if line == "\\ No newline at end of file" {
            let previous = body
                .last_mut()
                .ok_or_else(|| NormalizeError::MalformedPatch {
                    line: index + 1,
                    reason: "newline marker has no preceding patch line".to_owned(),
                })?;
            if previous.no_newline {
                return malformed(index + 1, "duplicate newline marker");
            }
            previous.no_newline = true;
        } else {
            let (kind, text) = match line.as_bytes().first() {
                Some(b' ') => (LineKind::Context, &line[1..]),
                Some(b'-') => (LineKind::Remove, &line[1..]),
                Some(b'+') => (LineKind::Add, &line[1..]),
                _ => return malformed(index + 1, "invalid hunk body line"),
            };
            let consumes_old = kind != LineKind::Add;
            let consumes_new = kind != LineKind::Remove;
            if (consumes_old && actual_old == old_count)
                || (consumes_new && actual_new == new_count)
            {
                return malformed(index + 1, "hunk projection exceeds declared count");
            }
            let projections = usize::from(consumes_old) + usize::from(consumes_new);
            *decoded_content_bytes = decoded_content_bytes
                .checked_add(
                    text.len()
                        .checked_add(1)
                        .and_then(|length| length.checked_mul(projections))
                        .ok_or(crate::workspace::edit::ir::IrError::Allocation)?,
                )
                .ok_or(crate::workspace::edit::ir::IrError::Allocation)?;
            if *decoded_content_bytes > max_content_bytes.saturating_add(2) {
                return Err(crate::workspace::edit::ir::IrError::ContentLimit {
                    actual: *decoded_content_bytes,
                    limit: max_content_bytes,
                }
                .into());
            }
            body.try_reserve(1)
                .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
            body.push(PatchLine {
                kind,
                text,
                no_newline: false,
            });
            actual_old += usize::from(consumes_old);
            actual_new += usize::from(consumes_new);
        }
        index += 1;
    }
    if lines.get(index) == Some(&"\\ No newline at end of file") {
        let previous = body
            .last_mut()
            .ok_or_else(|| NormalizeError::MalformedPatch {
                line: index + 1,
                reason: "newline marker has no preceding patch line".to_owned(),
            })?;
        if previous.no_newline {
            return malformed(index + 1, "duplicate newline marker");
        }
        previous.no_newline = true;
        index += 1;
    }
    for (line_index, line) in body.iter().enumerate().filter(|(_, line)| line.no_newline) {
        let later = &body[line_index + 1..];
        if (line.kind != LineKind::Add && later.iter().any(|later| later.kind != LineKind::Add))
            || (line.kind != LineKind::Remove
                && later.iter().any(|later| later.kind != LineKind::Remove))
        {
            return malformed(
                start + line_index + 2,
                "newline marker is not on a final projected line",
            );
        }
    }
    let omitted_newlines = body
        .iter()
        .filter(|line| line.no_newline)
        .map(|line| {
            usize::from(line.kind != LineKind::Add) + usize::from(line.kind != LineKind::Remove)
        })
        .sum::<usize>();
    *decoded_content_bytes = decoded_content_bytes
        .checked_sub(omitted_newlines)
        .ok_or(crate::workspace::edit::ir::IrError::Allocation)?;
    if *decoded_content_bytes > max_content_bytes {
        return Err(crate::workspace::edit::ir::IrError::ContentLimit {
            actual: *decoded_content_bytes,
            limit: max_content_bytes,
        }
        .into());
    }
    Ok((
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: body,
        },
        index,
    ))
}

fn count_operations(
    count: &mut usize,
    additional: usize,
    context: &NormalizationContext,
) -> Result<(), NormalizeError> {
    *count = count
        .checked_add(additional)
        .ok_or(crate::workspace::edit::ir::IrError::Allocation)?;
    if *count > context.limits().max_operations {
        return Err(crate::workspace::edit::ir::IrError::OperationLimit {
            actual: *count,
            limit: context.limits().max_operations,
        }
        .into());
    }
    Ok(())
}

fn parse_range(value: Option<&str>, line: usize) -> Result<(usize, usize), NormalizeError> {
    let value = value.ok_or_else(|| NormalizeError::MalformedPatch {
        line,
        reason: "missing hunk range".to_owned(),
    })?;
    let unsigned = &value[1..];
    let (start, count) = unsigned.split_once(',').unwrap_or((unsigned, "1"));
    let start = start.parse().map_err(|_| NormalizeError::MalformedPatch {
        line,
        reason: "invalid hunk start".to_owned(),
    })?;
    let count = count.parse().map_err(|_| NormalizeError::MalformedPatch {
        line,
        reason: "invalid hunk count".to_owned(),
    })?;
    Ok((start, count))
}

fn validate_hunk_coordinates(hunks: &[Hunk<'_>], line: usize) -> Result<(), NormalizeError> {
    let mut previous_old = None;
    let mut previous_new = None;
    let mut delta = 0_isize;
    for hunk in hunks {
        if (hunk.old_count != 0 && hunk.old_start == 0)
            || (hunk.new_count != 0 && hunk.new_start == 0)
        {
            return malformed(line, "non-empty hunk range starts at line zero");
        }
        let old_end = hunk.old_start.checked_add(hunk.old_count).ok_or_else(|| {
            NormalizeError::MalformedPatch {
                line,
                reason: "old hunk coordinates overflow".to_owned(),
            }
        })?;
        let new_end = hunk.new_start.checked_add(hunk.new_count).ok_or_else(|| {
            NormalizeError::MalformedPatch {
                line,
                reason: "new hunk coordinates overflow".to_owned(),
            }
        })?;
        let old_before = hunk.old_start - usize::from(hunk.old_count != 0);
        let new_before = hunk.new_start - usize::from(hunk.new_count != 0);
        if old_before.checked_add_signed(delta) != Some(new_before) {
            return malformed(line, "old and new hunk coordinates are inconsistent");
        }
        if previous_old.is_some_and(|end| hunk.old_start < end)
            || previous_new.is_some_and(|end| hunk.new_start < end)
        {
            return malformed(line, "hunk coordinate sequences overlap or move backwards");
        }
        previous_old = Some(old_end);
        previous_new = Some(new_end);
        delta = delta
            .checked_add_unsigned(hunk.new_count)
            .and_then(|delta| delta.checked_sub_unsigned(hunk.old_count))
            .ok_or_else(|| NormalizeError::MalformedPatch {
                line,
                reason: "hunk coordinate delta overflows".to_owned(),
            })?;
    }
    Ok(())
}

fn parse_git_paths(
    line: &str,
    line_number: usize,
    context: &NormalizationContext,
) -> Result<(RootRelativePath, RootRelativePath), NormalizeError> {
    let mut paths = line
        .strip_prefix("diff --git ")
        .expect("caller checked git header")
        .split_whitespace();
    let old = paths.next().and_then(|path| path.strip_prefix("a/"));
    let new = paths.next().and_then(|path| path.strip_prefix("b/"));
    if paths.next().is_some() || old.is_none() || new.is_none() {
        return malformed(line_number, "invalid or quoted diff --git paths");
    }
    Ok((
        parse_path(old.unwrap(), false, context)?,
        parse_path(new.unwrap(), false, context)?,
    ))
}

fn check_git_paths(
    git_paths: Option<&(RootRelativePath, RootRelativePath)>,
    old: Option<&RootRelativePath>,
    new: Option<&RootRelativePath>,
    line: usize,
) -> Result<(), NormalizeError> {
    if let Some((git_old, git_new)) = git_paths
        && (old != Some(git_old) || new != Some(git_new))
    {
        return malformed(line, "diff --git and section paths disagree");
    }
    Ok(())
}

fn set_mode(slot: &mut Option<bool>, mode: bool, line: usize) -> Result<(), NormalizeError> {
    if slot.replace(mode).is_some() {
        return malformed(line, "duplicate mode metadata");
    }
    Ok(())
}

fn is_binary_marker(line: &str) -> bool {
    line == "GIT binary patch"
        || line
            .strip_prefix("Binary files ")
            .and_then(|line| line.strip_suffix(" differ"))
            .is_some_and(|paths| paths.split_once(" and ").is_some())
}

fn lower_section(
    section: Section<'_>,
    context: &NormalizationContext,
    output: &mut Vec<EditOperation>,
    content_bytes: &mut usize,
) -> Result<(), NormalizeError> {
    if let Some((from, to)) = section.rename {
        let base = context
            .file(&from)
            .ok_or_else(|| NormalizeError::MissingBase(from.to_string()))?;
        if context.file(&to).is_some() {
            return Err(NormalizeError::UnexpectedBase(to.to_string()));
        }
        if section
            .old_executable
            .is_some_and(|mode| mode != base.executable())
            || section
                .new_executable
                .is_some_and(|mode| mode != base.executable())
        {
            return Err(NormalizeError::UnsupportedPatch(
                "rename with inconsistent or changed mode".to_owned(),
            ));
        }
        output.push(EditOperation::MoveFile {
            from,
            to,
            base_digest: base.digest().clone(),
        });
        return Ok(());
    }
    match (section.old_path.clone(), section.new_path.clone()) {
        (None, Some(path)) => lower_add(path, section, context, output, content_bytes),
        (Some(path), None) => lower_delete(path, section, context, output, content_bytes),
        (Some(old), Some(new)) if old == new => {
            lower_modify(old, section, context, output, content_bytes)
        }
        (Some(_), Some(_)) => Err(NormalizeError::UnsupportedPatch(
            "path changes require explicit rename metadata".to_owned(),
        )),
        (None, None) => Err(NormalizeError::UnsupportedPatch(
            "both file headers name /dev/null".to_owned(),
        )),
    }
}

fn lower_add(
    path: RootRelativePath,
    section: Section<'_>,
    context: &NormalizationContext,
    output: &mut Vec<EditOperation>,
    content_bytes: &mut usize,
) -> Result<(), NormalizeError> {
    if context.file(&path).is_some() {
        return Err(NormalizeError::UnexpectedBase(path.to_string()));
    }
    let ranges = lower_hunks(
        &path,
        &TextContent::empty(context.default_newline()),
        &section.hunks,
        context,
        content_bytes,
    )?;
    let bytes = apply_ranges(Vec::new(), &ranges, context.limits().max_content_bytes)?;
    output.push(EditOperation::AddFile {
        path,
        content: TextContent::from_bytes(&bytes)?,
        executable: section.new_executable.unwrap_or(false),
    });
    Ok(())
}

fn lower_delete(
    path: RootRelativePath,
    section: Section<'_>,
    context: &NormalizationContext,
    output: &mut Vec<EditOperation>,
    content_bytes: &mut usize,
) -> Result<(), NormalizeError> {
    let base = context
        .file(&path)
        .ok_or_else(|| NormalizeError::MissingBase(path.to_string()))?;
    if section
        .old_executable
        .is_some_and(|mode| mode != base.executable())
    {
        return Err(NormalizeError::BaseMismatch(path.to_string()));
    }
    let ranges = lower_hunks(
        &path,
        base.content(),
        &section.hunks,
        context,
        content_bytes,
    )?;
    if !apply_ranges(
        base.content()
            .try_render(context.limits().max_content_bytes)?,
        &ranges,
        context.limits().max_content_bytes,
    )?
    .is_empty()
    {
        return Err(NormalizeError::BaseMismatch(path.to_string()));
    }
    output.push(EditOperation::DeleteFile {
        path,
        base_digest: base.digest().clone(),
    });
    Ok(())
}

fn lower_modify(
    path: RootRelativePath,
    section: Section<'_>,
    context: &NormalizationContext,
    output: &mut Vec<EditOperation>,
    content_bytes: &mut usize,
) -> Result<(), NormalizeError> {
    let base = context
        .file(&path)
        .ok_or_else(|| NormalizeError::MissingBase(path.to_string()))?;
    if section
        .old_executable
        .is_some_and(|mode| mode != base.executable())
    {
        return Err(NormalizeError::BaseMismatch(path.to_string()));
    }
    let mut ranges = lower_hunks(
        &path,
        base.content(),
        &section.hunks,
        context,
        content_bytes,
    )?;
    if ranges.is_empty() {
        ranges.push((
            ByteRange::new(0, 0)?,
            TextContent::empty(base.content().newline()),
            TextContent::empty(base.content().newline()),
        ));
    }
    let executable = executable_mode(
        section.old_executable.unwrap_or(base.executable()),
        section.new_executable.unwrap_or(base.executable()),
    );
    for (range, expected, replacement) in ranges {
        output.push(EditOperation::ReplaceRange {
            path: path.clone(),
            base_digest: base.digest().clone(),
            range,
            expected,
            replacement,
            executable,
        });
    }
    Ok(())
}

type LoweredRange = (ByteRange, TextContent, TextContent);

fn lower_hunks(
    path: &RootRelativePath,
    base: &TextContent,
    hunks: &[Hunk<'_>],
    context: &NormalizationContext,
    content_bytes: &mut usize,
) -> Result<Vec<LoweredRange>, NormalizeError> {
    let bytes = base.try_render(context.limits().max_content_bytes)?;
    let base_lines = split_lines(&bytes, base.newline())?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(hunks.len())
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    let mut previous: Option<ByteRange> = None;
    for hunk in hunks {
        let line_index = if hunk.old_count == 0 {
            hunk.old_start
        } else {
            hunk.old_start
                .checked_sub(1)
                .ok_or_else(|| NormalizeError::BaseMismatch(path.to_string()))?
        };
        if line_index > base_lines.len() || line_index + hunk.old_count > base_lines.len() {
            return Err(NormalizeError::BaseMismatch(path.to_string()));
        }
        let mut old_lines = Vec::new();
        old_lines
            .try_reserve_exact(hunk.old_count)
            .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
        old_lines.extend(hunk.lines.iter().filter(|line| line.kind != LineKind::Add));
        for (patch, actual) in old_lines
            .iter()
            .zip(&base_lines[line_index..line_index + hunk.old_count])
        {
            if patch.text.as_bytes() != &bytes[actual.content_start..actual.content_end]
                || actual.has_newline == patch.no_newline
            {
                return Err(NormalizeError::BaseMismatch(path.to_string()));
            }
        }
        let start = if hunk.old_count == 0 {
            base_lines
                .get(line_index)
                .map_or(bytes.len(), |line| line.content_start)
        } else {
            base_lines[line_index].content_start
        };
        let end = if hunk.old_count == 0 {
            start
        } else {
            base_lines[line_index + hunk.old_count - 1].line_end
        };
        let mut new_lines = Vec::new();
        new_lines
            .try_reserve_exact(hunk.new_count)
            .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
        new_lines.extend(
            hunk.lines
                .iter()
                .filter(|line| line.kind != LineKind::Remove),
        );
        if new_lines.len() != hunk.new_count {
            return Err(NormalizeError::BaseMismatch(path.to_string()));
        }
        let expected_len = end - start;
        let replacement_len = patch_lines_len(&new_lines, base.newline())?;
        *content_bytes = content_bytes
            .checked_add(expected_len)
            .and_then(|total| total.checked_add(replacement_len))
            .ok_or(crate::workspace::edit::ir::IrError::Allocation)?;
        if *content_bytes > context.limits().max_content_bytes {
            return Err(crate::workspace::edit::ir::IrError::ContentLimit {
                actual: *content_bytes,
                limit: context.limits().max_content_bytes,
            }
            .into());
        }
        let replacement = render_patch_lines(
            &new_lines,
            base.newline(),
            context.limits().max_content_bytes,
        )?;
        let range = ByteRange::new(start, end)?;
        if previous.is_some_and(|previous| {
            range.start < previous.end
                || (range.start == previous.start
                    && (range.start == range.end || previous.start == previous.end))
        }) {
            return Err(NormalizeError::Ir(
                crate::workspace::edit::ir::IrError::OverlappingRanges(path.to_string()),
            ));
        }
        previous = Some(range);
        output.push((
            range,
            TextContent::from_bytes(&bytes[start..end])?,
            replacement,
        ));
    }
    Ok(output)
}

#[derive(Clone, Copy)]
struct BaseLine {
    content_start: usize,
    content_end: usize,
    line_end: usize,
    has_newline: bool,
}

fn split_lines(bytes: &[u8], newline: Newline) -> Result<Vec<BaseLine>, NormalizeError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let separator: &[u8] = match newline {
        Newline::Lf => b"\n",
        Newline::Crlf => b"\r\n",
    };
    let mut lines = Vec::new();
    lines
        .try_reserve_exact(
            bytes
                .windows(separator.len())
                .filter(|window| *window == separator)
                .count()
                + 1,
        )
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    let mut start = 0;
    while start < bytes.len() {
        let relative = bytes[start..]
            .windows(separator.len())
            .position(|window| window == separator);
        if let Some(relative) = relative {
            let content_end = start + relative;
            let line_end = content_end + separator.len();
            lines.push(BaseLine {
                content_start: start,
                content_end,
                line_end,
                has_newline: true,
            });
            start = line_end;
        } else {
            lines.push(BaseLine {
                content_start: start,
                content_end: bytes.len(),
                line_end: bytes.len(),
                has_newline: false,
            });
            break;
        }
    }
    Ok(lines)
}

fn patch_lines_len(lines: &[&PatchLine<'_>], newline: Newline) -> Result<usize, NormalizeError> {
    let separator_len = match newline {
        Newline::Lf => 1,
        Newline::Crlf => 2,
    };
    lines.iter().try_fold(0_usize, |total, line| {
        total
            .checked_add(line.text.len())
            .and_then(|total| total.checked_add(if line.no_newline { 0 } else { separator_len }))
            .ok_or_else(|| crate::workspace::edit::ir::IrError::Allocation.into())
    })
}

fn render_patch_lines(
    lines: &[&PatchLine<'_>],
    newline: Newline,
    max_bytes: usize,
) -> Result<TextContent, NormalizeError> {
    let separator = match newline {
        Newline::Lf => "\n",
        Newline::Crlf => "\r\n",
    };
    let rendered_len = patch_lines_len(lines, newline)?;
    if rendered_len > max_bytes {
        return Err(crate::workspace::edit::ir::IrError::ContentLimit {
            actual: rendered_len,
            limit: max_bytes,
        }
        .into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(rendered_len)
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    for line in lines {
        bytes.extend_from_slice(line.text.as_bytes());
        if !line.no_newline {
            bytes.extend_from_slice(separator.as_bytes());
        }
    }
    TextContent::from_bytes(&bytes).map_err(Into::into)
}

fn apply_ranges(
    mut bytes: Vec<u8>,
    ranges: &[LoweredRange],
    max_bytes: usize,
) -> Result<Vec<u8>, NormalizeError> {
    let final_len = ranges
        .iter()
        .try_fold(bytes.len(), |length, (range, _, replacement)| {
            length
                .checked_sub((range.end - range.start) as usize)
                .and_then(|length| length.checked_add(replacement.rendered_len()))
                .ok_or(crate::workspace::edit::ir::IrError::Allocation)
        })?;
    if final_len > max_bytes {
        return Err(crate::workspace::edit::ir::IrError::ContentLimit {
            actual: final_len,
            limit: max_bytes,
        }
        .into());
    }
    bytes
        .try_reserve(final_len.saturating_sub(bytes.len()))
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    for (range, _, replacement) in ranges.iter().rev() {
        bytes.splice(
            range.start as usize..range.end as usize,
            replacement.try_render(max_bytes)?,
        );
    }
    Ok(bytes)
}

fn parse_header_path(
    line: &str,
    prefix: &str,
    strip_git_prefix: bool,
    context: &NormalizationContext,
) -> Result<Option<RootRelativePath>, NormalizeError> {
    let value = line
        .strip_prefix(prefix)
        .expect("caller checked header prefix")
        .split_once('\t')
        .map_or_else(|| line.strip_prefix(prefix).unwrap(), |(path, _)| path);
    if value == "/dev/null" {
        return Ok(None);
    }
    parse_path(value, strip_git_prefix, context).map(Some)
}

fn parse_path(
    value: &str,
    strip_git_prefix: bool,
    context: &NormalizationContext,
) -> Result<RootRelativePath, NormalizeError> {
    if value.starts_with('"') || value.ends_with('"') {
        return Err(NormalizeError::UnsupportedPatch(
            "quoted or escaped paths are unsupported".to_owned(),
        ));
    }
    let value = if strip_git_prefix {
        value
            .strip_prefix("a/")
            .or_else(|| value.strip_prefix("b/"))
            .unwrap_or(value)
    } else {
        value
    };
    if value.len() > context.limits().max_path_bytes {
        return Err(crate::workspace::edit::ir::IrError::InvalidPath(
            "path exceeds active byte limit".to_owned(),
        )
        .into());
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| crate::workspace::edit::ir::IrError::Allocation)?;
    owned.push_str(value);
    RootRelativePath::parse(owned, context.limits().max_path_bytes).map_err(Into::into)
}

fn parse_mode(value: &str, line: usize) -> Result<bool, NormalizeError> {
    match value {
        "100644" => Ok(false),
        "100755" => Ok(true),
        _ => malformed(line, "unsupported file mode"),
    }
}

fn malformed<T>(line: usize, reason: &str) -> Result<T, NormalizeError> {
    Err(NormalizeError::MalformedPatch {
        line,
        reason: reason.to_owned(),
    })
}
