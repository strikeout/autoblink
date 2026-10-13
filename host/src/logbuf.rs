//! A ring buffer of log lines. The tracing layer writes it; the UI reads it.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

/// Max log lines retained for the UI log panel.
const LOG_CAPACITY: usize = 200;

#[derive(Debug, Default)]
struct Ring {
    lines: VecDeque<String>,
    /// The number of lines pushed since the start. It is the sequence number of the next line.
    total: u64,
}

/// Thread-safe ring buffer of log lines, written by the tracing layer and
/// read by the UI.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<Ring>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a log line (called from the tracing writer).
    pub fn push(&self, line: String) {
        let mut ring = self.inner.lock();
        if ring.lines.len() >= LOG_CAPACITY {
            ring.lines.pop_front();
        }
        ring.lines.push_back(line);
        ring.total += 1;
    }

    /// Snapshot all lines for rendering.
    pub fn lines(&self) -> Vec<String> {
        self.inner.lock().lines.iter().cloned().collect()
    }

    /// Returns the lines with a sequence number of `seq` or more, and the next sequence number.
    ///
    /// Lines that left the ring are skipped. Start with `seq` = 0 to get all retained lines.
    pub fn since(&self, seq: u64) -> (Vec<String>, u64) {
        let ring = self.inner.lock();
        let first = ring.total - ring.lines.len() as u64;
        let skip = seq.saturating_sub(first) as usize;
        (ring.lines.iter().skip(skip).cloned().collect(), ring.total)
    }
}

/// A `std::io::Write` implementation that routes tracing output into the
/// `LogBuffer` instead of stdout.  Each `write()` call appends to a
/// line buffer; newlines flush complete lines into the ring.
#[derive(Clone)]
pub struct LogWriter {
    buf: LogBuffer,
    partial: String,
}

impl LogWriter {
    pub fn new(buf: LogBuffer) -> Self {
        Self {
            buf,
            partial: String::new(),
        }
    }
}

impl std::io::Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let chunk = String::from_utf8_lossy(data);
        self.partial.push_str(&chunk);

        let mut it = self.partial.split('\n').peekable();
        let mut new_partial: Option<String> = None;

        while let Some(part) = it.next() {
            let is_last = it.peek().is_none();
            if is_last {
                new_partial = Some(part.to_string());
                break;
            }

            let line = part.trim_end_matches('\r');
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                self.buf.push(trimmed.to_string());
            }
        }

        self.partial = new_partial.unwrap_or_default();
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn log_buffer_clones_share_inner_ring() {
        let a = LogBuffer::new();
        let b = a.clone();

        assert!(Arc::ptr_eq(&a.inner, &b.inner));

        b.push("hello".to_string());
        assert_eq!(a.lines(), vec!["hello".to_string()]);
    }

    #[test]
    fn since_returns_only_new_lines_and_skips_evicted_ones() {
        let buf = LogBuffer::new();
        buf.push("a".into());
        buf.push("b".into());
        let (lines, next) = buf.since(0);
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(next, 2);

        buf.push("c".into());
        assert_eq!(buf.since(next), (vec!["c".to_string()], 3));
        assert_eq!(buf.since(3), (Vec::<String>::new(), 3));

        for i in 0..LOG_CAPACITY {
            buf.push(i.to_string());
        }
        // "a", "b" and "c" left the ring, so a reader at seq 1 gets the retained lines only.
        let (lines, next) = buf.since(1);
        assert_eq!(lines.len(), LOG_CAPACITY);
        assert_eq!(lines[0], "0");
        assert_eq!(next, 3 + LOG_CAPACITY as u64);
    }

    #[test]
    fn log_writer_buffers_partial_lines_until_newline() {
        let buf = LogBuffer::new();
        let mut w = LogWriter::new(buf.clone());

        w.write_all(b"hello ").unwrap();
        assert_eq!(buf.lines(), Vec::<String>::new());

        w.write_all(b"world\n").unwrap();
        assert_eq!(buf.lines(), vec!["hello world".to_string()]);
    }
}

/// Wrapper that implements `tracing_subscriber::fmt::MakeWriter` so we can
/// plug `LogWriter` into the tracing subscriber.
#[derive(Clone)]
pub struct MakeLogWriter {
    writer: LogWriter,
}

impl MakeLogWriter {
    pub fn new(buf: LogBuffer) -> Self {
        Self {
            writer: LogWriter::new(buf),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeLogWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.writer.clone()
    }
}
