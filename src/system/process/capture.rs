//! Bounded byte-exact sinks for ProcessKit raw tees.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retention {
    Prefix,
    Tail,
}

#[derive(Debug)]
struct CaptureState {
    bytes: Vec<u8>,
    max_bytes: usize,
    total_bytes: u64,
    retention: Retention,
    complete: bool,
}

/// A snapshot of one independently bounded raw stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedBytes {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
    pub complete: bool,
}

/// An [`AsyncWrite`] sink whose clones share a bounded byte capture.
#[derive(Clone, Debug)]
pub(crate) struct BoundedCapture {
    state: Arc<Mutex<CaptureState>>,
}

impl BoundedCapture {
    pub(crate) fn tail(max_bytes: usize) -> Self {
        Self::new(max_bytes, Retention::Tail)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn prefix(max_bytes: usize) -> Self {
        Self::new(max_bytes, Retention::Prefix)
    }

    fn new(max_bytes: usize, retention: Retention) -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureState {
                bytes: Vec::with_capacity(max_bytes),
                max_bytes,
                total_bytes: 0,
                retention,
                complete: false,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CaptureState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn snapshot(&self) -> CapturedBytes {
        let state = self.lock();
        CapturedBytes {
            bytes: state.bytes.clone(),
            total_bytes: state.total_bytes,
            truncated: state.total_bytes > state.bytes.len() as u64,
            complete: state.complete,
        }
    }

    pub(crate) fn accept(&self, bytes: &[u8]) {
        let mut state = self.lock();
        state.complete = false;
        state.total_bytes = state.total_bytes.saturating_add(bytes.len() as u64);
        let remaining = state.max_bytes.saturating_sub(state.bytes.len());
        match state.retention {
            Retention::Prefix => state
                .bytes
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]),
            Retention::Tail if state.max_bytes == 0 => {}
            Retention::Tail if bytes.len() >= state.max_bytes => {
                let start = bytes.len() - state.max_bytes;
                state.bytes.clear();
                state.bytes.extend_from_slice(&bytes[start..]);
            }
            Retention::Tail => {
                let overflow = state
                    .bytes
                    .len()
                    .saturating_add(bytes.len())
                    .saturating_sub(state.max_bytes);
                if overflow != 0 {
                    state.bytes.drain(..overflow);
                }
                state.bytes.extend_from_slice(bytes);
            }
        }
    }

    pub(crate) fn mark_complete(&self) {
        self.lock().complete = true;
    }
}

impl AsyncWrite for BoundedCapture {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.accept(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.mark_complete();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn tail_is_bounded_and_counts_every_byte() {
        let capture = BoundedCapture::tail(4);
        let mut writer = capture.clone();
        writer.write_all(b"abc").await.unwrap();
        writer.write_all(b"defgh").await.unwrap();
        assert_eq!(
            capture.snapshot(),
            CapturedBytes {
                bytes: b"efgh".to_vec(),
                total_bytes: 8,
                truncated: true,
                complete: false,
            }
        );
    }

    #[tokio::test]
    async fn prefix_is_bounded_and_counts_every_byte() {
        let capture = BoundedCapture::prefix(4);
        let mut writer = capture.clone();
        writer.write_all(b"abc").await.unwrap();
        writer.write_all(b"defgh").await.unwrap();
        assert_eq!(capture.snapshot().bytes, b"abcd");
        assert_eq!(capture.snapshot().total_bytes, 8);
    }
}
