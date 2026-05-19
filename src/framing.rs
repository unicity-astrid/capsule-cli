//! Length-prefixed envelope framing for the CLI uplink protocol.
//!
//! The kernel ↔ CLI client wire format is `[4-byte u32 BE length] +
//! [payload]`. Previously this lived in the host as the `net-read` /
//! `net-write` ABI; now it's a user-space concern (the host only exposes
//! `std::net::TcpStream`-shaped byte-stream primitives, mirroring the
//! `std::net` / OS split).
//!
//! [`FramedStream`] wraps a [`astrid_sdk::net::StreamHandle`] plus
//! per-stream receive state so [`FramedStream::try_recv`] can resume a
//! partially-read frame across polling iterations without losing bytes.

use astrid_sdk::net::{StreamHandle, close, read_bytes, set_read_timeout, write_bytes};
use astrid_sdk::prelude::*;
use std::time::Duration;

/// Polling timeout applied to the underlying stream when `try_recv`
/// runs. Lets a single iteration return promptly when no data is
/// pending while still amortising the host-fn call cost across many
/// short reads.
const POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Per-frame payload cap, matching the kernel's `MAX_BYTES_PER_CALL`
/// on byte-stream reads. Frames larger than this are a protocol
/// violation; we surface that as a transport error rather than
/// allocating unboundedly.
const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;

/// Errors from [`FramedStream::try_recv`] / [`FramedStream::send`].
#[derive(Debug)]
pub(crate) enum FramedError {
    /// Peer disconnected cleanly (EOF) or mid-frame.
    Closed,
    /// Transport error (timeout, OOM, protocol violation).
    Io(String),
}

impl core::fmt::Display for FramedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => write!(f, "stream closed"),
            Self::Io(msg) => write!(f, "{msg}"),
        }
    }
}

/// Receive state for a single stream's incoming framed-message reader.
enum RxState {
    /// Accumulating the 4-byte length prefix.
    Length { buf: [u8; 4], filled: usize },
    /// Accumulating the payload of declared length.
    Payload { len: usize, buf: Vec<u8> },
}

impl RxState {
    fn new() -> Self {
        Self::Length {
            buf: [0u8; 4],
            filled: 0,
        }
    }
}

/// A [`StreamHandle`] augmented with the receive state needed to
/// reassemble length-prefixed frames across polling iterations.
pub(crate) struct FramedStream {
    handle: StreamHandle,
    rx: RxState,
    /// Whether the host-side read-timeout has been installed on this
    /// stream. Done lazily on first `try_recv` to avoid the host fn
    /// call for streams that are only used to send.
    poll_timeout_installed: bool,
}

impl FramedStream {
    pub(crate) fn new(handle: StreamHandle) -> Self {
        Self {
            handle,
            rx: RxState::new(),
            poll_timeout_installed: false,
        }
    }

    /// Try to advance the receive state machine, returning a complete
    /// frame if one is now buffered.
    ///
    /// - `Ok(Some(frame))` — a full frame is ready; state resets for
    ///   the next frame.
    /// - `Ok(None)` — no progress beyond a partial read; call again.
    /// - `Err(Closed)` — peer disconnected.
    /// - `Err(Io(msg))` — transport-level error.
    pub(crate) fn try_recv(&mut self) -> Result<Option<Vec<u8>>, FramedError> {
        if !self.poll_timeout_installed {
            set_read_timeout(&self.handle, Some(POLL_TIMEOUT))
                .map_err(|e| FramedError::Io(format!("set_read_timeout: {e}")))?;
            self.poll_timeout_installed = true;
        }

        loop {
            match &mut self.rx {
                RxState::Length { buf, filled } => {
                    let want = u32::try_from(4 - *filled).expect("filled <= 4 by construction");
                    match read_bytes(&self.handle, want) {
                        Ok(chunk) if chunk.is_empty() => return Err(FramedError::Closed),
                        Ok(chunk) => {
                            let n = chunk.len();
                            buf[*filled..*filled + n].copy_from_slice(&chunk);
                            *filled += n;
                            if *filled == 4 {
                                let len = u32::from_be_bytes(*buf) as usize;
                                if len > MAX_FRAME_BYTES {
                                    return Err(FramedError::Io(format!(
                                        "frame too large: {len} bytes (max {MAX_FRAME_BYTES})"
                                    )));
                                }
                                self.rx = RxState::Payload {
                                    len,
                                    buf: Vec::with_capacity(len),
                                };
                            }
                        }
                        Err(e) if would_block(&e) => return Ok(None),
                        Err(e) => return Err(FramedError::Io(e.to_string())),
                    }
                }
                RxState::Payload { len, buf } => {
                    let want = u32::try_from(*len - buf.len())
                        .map_err(|_| FramedError::Io("payload size overflow".into()))?;
                    if want == 0 {
                        let frame = core::mem::take(buf);
                        self.rx = RxState::new();
                        return Ok(Some(frame));
                    }
                    match read_bytes(&self.handle, want) {
                        Ok(chunk) if chunk.is_empty() => return Err(FramedError::Closed),
                        Ok(chunk) => {
                            buf.extend_from_slice(&chunk);
                            if buf.len() == *len {
                                let frame = core::mem::take(buf);
                                self.rx = RxState::new();
                                return Ok(Some(frame));
                            }
                        }
                        Err(e) if would_block(&e) => return Ok(None),
                        Err(e) => return Err(FramedError::Io(e.to_string())),
                    }
                }
            }
        }
    }

    /// Write one length-prefixed frame, blocking until the bytes have
    /// been written or the peer disconnects.
    pub(crate) fn send(&self, data: &[u8]) -> Result<(), FramedError> {
        let len = u32::try_from(data.len())
            .map_err(|_| FramedError::Io("frame too large for u32 length prefix".into()))?;
        write_all(&self.handle, &len.to_be_bytes())?;
        write_all(&self.handle, data)?;
        Ok(())
    }

    /// Close the underlying stream. Idempotent.
    pub(crate) fn close(self) {
        let _ = close(&self.handle);
    }
}

/// Detect the host's `"would block"` sentinel without depending on a
/// specific [`SysError`] variant string.
fn would_block(err: &SysError) -> bool {
    err.to_string().contains("would block")
}

/// Write every byte of `data` or return [`FramedError::Closed`].
/// `write_bytes` may write fewer bytes than requested when the
/// kernel-side socket buffer fills; loop until drained.
fn write_all(handle: &StreamHandle, mut data: &[u8]) -> Result<(), FramedError> {
    while !data.is_empty() {
        match write_bytes(handle, data) {
            Ok(0) => return Err(FramedError::Closed),
            Ok(n) => {
                data = &data[n as usize..];
            }
            Err(e) => return Err(FramedError::Io(e.to_string())),
        }
    }
    Ok(())
}
