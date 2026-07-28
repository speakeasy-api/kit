//! Minimal Server-Sent Events framer.
//!
//! Anthropic's streaming Messages API sends `event: <name>\ndata: <json>\n\n`
//! records. We deliberately avoid pulling in a full SSE crate: this framer is
//! ~100 lines and exactly as forgiving as the spec demands (LF or CRLF record
//! terminators, comment lines, multi-line `data:` values).
//!
//! Events produced here are consumed by [`crate::stream::EventTranslator`].

/// An individual parsed SSE record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// Value from the `event:` line, or `"message"` if absent (the SSE spec
    /// default — but Anthropic always names its events so this rarely hits).
    pub(crate) name: String,
    /// Concatenated data payload. Multi-line `data:` values are joined by
    /// newlines per the SSE spec; Anthropic sends one-line JSON payloads.
    pub(crate) data: String,
}

/// Chunked byte decoder that frames SSE records.
///
/// Feed it bytes from the HTTP body stream via [`SseDecoder::feed`]; each call
/// returns any events that became complete. Records are terminated by a blank
/// line (`\n\n` or `\r\n\r\n`).
#[derive(Default)]
pub(crate) struct SseDecoder {
    /// Bytes not yet parsed into a complete record.
    buffer: String,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Pushes `chunk` onto the decoder's buffer and extracts any records that
    /// have become complete.
    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut out = Vec::new();
        while let Some(end) = find_record_boundary(&self.buffer) {
            let record: String = self.buffer.drain(..end).collect();
            let record = record.trim_end_matches(&['\r', '\n'][..]).to_string();
            if record.is_empty() {
                continue;
            }
            if let Some(event) = parse_record(&record) {
                out.push(event);
            }
        }
        out
    }
}

/// Returns the end index (exclusive) of the first complete record in `buf`,
/// *including* its blank-line terminator.
fn find_record_boundary(buf: &str) -> Option<usize> {
    if let Some(idx) = buf.find("\n\n") {
        return Some(idx + 2);
    }
    if let Some(idx) = buf.find("\r\n\r\n") {
        return Some(idx + 4);
    }
    None
}

fn parse_record(record: &str) -> Option<SseEvent> {
    let mut event_name: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for raw_line in record.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') {
            // Comment or spec-blank line inside a record; SSE treats leading
            // `:` as a comment. Anthropic doesn't emit these but be lenient.
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Ignore other fields (id:, retry:, etc.); Anthropic doesn't use them.
    }
    if event_name.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        name: event_name.unwrap_or_else(|| "message".to_string()),
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_single_record() {
        let mut dec = SseDecoder::new();
        let events = dec.feed("event: ping\ndata: {}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "ping");
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn decodes_across_chunk_boundaries() {
        let mut dec = SseDecoder::new();
        let first = dec.feed("event: ping\nda");
        assert!(first.is_empty());
        let second = dec.feed("ta: {}\n\nevent: ping\ndata: {}\n\n");
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].data, "{}");
    }

    #[test]
    fn decodes_multiline_data() {
        let mut dec = SseDecoder::new();
        let events = dec.feed("event: foo\ndata: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn ignores_comment_lines() {
        let mut dec = SseDecoder::new();
        let events = dec.feed(": keepalive\nevent: ping\ndata: {}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "ping");
    }

    #[test]
    fn handles_crlf_terminators() {
        let mut dec = SseDecoder::new();
        let events = dec.feed("event: ping\r\ndata: {}\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "ping");
    }
}
