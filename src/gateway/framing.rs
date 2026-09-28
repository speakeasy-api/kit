//! Cancellation-safe bounded framing for the supervised ACP child's stdout.
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

pub(super) struct Lines<R> {
    reader: BufReader<R>,
    partial: Vec<u8>,
}
impl<R: AsyncRead + Unpin> Lines<R> {
    pub(super) fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            partial: Vec::new(),
        }
    }
    pub(super) async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |position| position + 1);
            if self.partial.len().saturating_add(count) > super::MAX_REPLAY {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ACP child frame exceeds replay limit",
                ));
            }
            self.partial.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if newline.is_some() || count == 0 {
                if self.partial.is_empty() {
                    return Ok(None);
                }
                let bytes = std::mem::take(&mut self.partial);
                return String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn cancellation_preserves_partial_frame() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut lines = Lines::new(reader);
        writer.write_all(b"first").await.unwrap();
        let mut next = Box::pin(lines.next_line());
        assert!(futures_util::poll!(&mut next).is_pending());
        drop(next);
        writer.write_all(b" frame\nsecond\n").await.unwrap();
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("first frame\n")
        );
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("second\n")
        );
        drop(writer);
        assert!(lines.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_unterminated_frame_is_rejected_before_parsing() {
        let bytes = vec![b'x'; crate::gateway::MAX_REPLAY + 1];
        let mut lines = Lines::new(bytes.as_slice());
        assert_eq!(
            lines.next_line().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(lines.partial.len() <= crate::gateway::MAX_REPLAY);
    }
}
