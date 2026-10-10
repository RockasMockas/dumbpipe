//! A tee log writer for the GUI.
//!
//! The CLI installs a plain stderr subscriber. The GUI installs this instead:
//! every formatted log line goes to stderr (exactly what the CLI shows) *and*
//! into a bounded ring buffer the log panel renders. This is a `MakeWriter`,
//! not a `registry` `Layer`, because `tracing-subscriber` is compiled without
//! the `registry` feature.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{Arc, Mutex},
};

use tracing_subscriber::{fmt::MakeWriter, EnvFilter};

/// The most log lines kept in the ring for the GUI log panel.
const MAX_LINES: usize = 2000;

/// A bounded, thread-safe ring of log lines.
pub type LogRing = Arc<Mutex<VecDeque<String>>>;

/// Create an empty ring.
pub fn new_ring() -> LogRing {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// A snapshot of the ring for rendering (cloned so the lock is never held by
/// the UI).
pub fn snapshot(ring: &LogRing) -> Vec<String> {
    ring.lock()
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default()
}

/// Push a manual line into the ring (used for GUI-originated messages that are
/// not produced by `tracing`).
pub fn push(ring: &LogRing, line: impl Into<String>) {
    if let Ok(mut ring) = ring.lock() {
        ring.push_back(line.into());
        while ring.len() > MAX_LINES {
            ring.pop_front();
        }
    }
}

/// A writer that forwards to stderr and collects complete lines into the ring.
pub(crate) struct TeeWriter {
    ring: LogRing,
    buf: Vec<u8>,
}

impl TeeWriter {
    /// Move any complete lines from the buffer into the ring.
    fn drain_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches('\n')
                .to_string();
            push(&self.ring, text);
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Mirror to stderr so the GUI process shows the same output as the CLI.
        io::stderr().write_all(bytes)?;
        self.buf.extend_from_slice(bytes);
        self.drain_lines();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Surface any trailing partial line without a newline.
        if !self.buf.is_empty() {
            let text = String::from_utf8_lossy(&self.buf).trim_end().to_string();
            push(&self.ring, text);
            self.buf.clear();
        }
        io::stderr().flush()
    }
}

/// A `MakeWriter` handing out [`TeeWriter`]s bound to one ring.
pub(crate) struct TeeMakeWriter {
    ring: LogRing,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            ring: self.ring.clone(),
            buf: Vec::new(),
        }
    }
}

/// Install the GUI logging: stderr plus the ring, at the given verbosity.
///
/// `RUST_LOG` wins if set, matching the CLI. ANSI is disabled so the ring
/// holds plain text for the log panel.
pub fn install(ring: LogRing, verbosity: u8) {
    let filter = EnvFilter::builder()
        .with_default_directive(crate::log_level(verbosity).into())
        .from_env_lossy();
    let tee = TeeMakeWriter { ring };
    // Ignore the "already installed" error: the GUI installs once, but a second
    // call (e.g. a stray re-init) must not panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(tee)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_splits_lines_into_ring() {
        let ring = new_ring();
        let mut w = TeeWriter {
            ring: ring.clone(),
            buf: Vec::new(),
        };
        w.write_all(b"first\nsecond").unwrap();
        w.flush().unwrap();
        let snap = snapshot(&ring);
        assert_eq!(snap, vec!["first", "second"]);
    }

    #[test]
    fn ring_is_bounded() {
        let ring = new_ring();
        for i in 0..(MAX_LINES + 50) {
            push(&ring, format!("line {i}"));
        }
        let snap = snapshot(&ring);
        assert_eq!(snap.len(), MAX_LINES);
        assert_eq!(snap.last().unwrap(), &format!("line {}", MAX_LINES + 49));
    }
}
