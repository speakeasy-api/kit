//! Minimal SSE framer for OpenAI-compatible chat completions streams.

/// A parsed SSE record. `name` is `None` for unnamed default frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) name: Option<String>,
    pub(crate) data: String,
}

/// Chunked byte decoder producing complete SSE records.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

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
    fn decodes_unnamed_done_frame() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed("data: [DONE]\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].name.is_none());
        assert_eq!(events[0].data, "[DONE]");
    }

    #[test]
    fn decodes_across_chunk_boundaries() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed("data: {\"choices\"").is_empty());
        let events = decoder.feed(":[]}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"choices\":[]}");
    }

    #[test]
    fn preserves_named_error_frame() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed("event: error\r\ndata: {\"error\":\"x\"}\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("error"));
        assert_eq!(events[0].data, "{\"error\":\"x\"}");
    }
}
