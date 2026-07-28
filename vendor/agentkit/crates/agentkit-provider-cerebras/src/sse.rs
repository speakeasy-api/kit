//! Minimal SSE framer for Cerebras streaming responses.
//!
//! Cerebras follows OpenAI's SSE dialect but preserves `event:` lines for
//! the error channel: most frames are unnamed `data:` lines, and the
//! terminator is `data: [DONE]`.

/// A parsed SSE record. `name` is `None` for unnamed (default) frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// Value from the `event:` line, or `None` when the frame was unnamed.
    pub(crate) name: Option<String>,
    /// Concatenated data payload. Multi-line `data:` values are joined with
    /// newlines per the SSE spec.
    pub(crate) data: String,
}

/// Chunked byte decoder producing [`SseEvent`]s.
///
/// Feed it bytes from the HTTP body stream via [`SseDecoder::feed`]; each call
/// returns every record that became complete.
#[derive(Default)]
pub(crate) struct SseDecoder {
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
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Ignore `id:`, `retry:`, etc.
    }
    if event_name.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        name: event_name,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_error_frame_preserved() {
        let mut d = SseDecoder::new();
        let events = d.feed("event: error\ndata: {\"oops\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("error"));
        assert_eq!(events[0].data, "{\"oops\":1}");
    }

    #[test]
    fn unnamed_frame_has_no_name() {
        let mut d = SseDecoder::new();
        let events = d.feed("data: {\"delta\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].name.is_none());
        assert_eq!(events[0].data, "{\"delta\":1}");
    }

    #[test]
    fn done_terminator_surfaces_verbatim() {
        let mut d = SseDecoder::new();
        let events = d.feed("data: [DONE]\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].name.is_none());
        assert_eq!(events[0].data, "[DONE]");
    }

    #[test]
    fn frame_split_mid_data_reassembles() {
        let mut d = SseDecoder::new();
        assert!(d.feed("data: {\"").is_empty());
        let events = d.feed("foo\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"foo\":1}");
    }

    #[test]
    fn frame_split_on_terminator_emits_once() {
        let mut d = SseDecoder::new();
        assert!(d.feed("data: {\"a\":1}").is_empty());
        let events = d.feed("\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn crlf_terminators_accepted() {
        let mut d = SseDecoder::new();
        let events = d.feed("event: error\r\ndata: {}\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("error"));
    }

    #[test]
    fn multiple_data_lines_joined() {
        let mut d = SseDecoder::new();
        let events = d.feed("data: a\ndata: b\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn trailing_buffer_not_emitted() {
        let mut d = SseDecoder::new();
        let events = d.feed("data: {\"a\":1}\n");
        assert!(events.is_empty());
    }

    #[test]
    fn unknown_prefix_lines_ignored() {
        let mut d = SseDecoder::new();
        let events = d.feed(": keepalive\nid: 1\nretry: 3000\ndata: {}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{}");
    }
}
