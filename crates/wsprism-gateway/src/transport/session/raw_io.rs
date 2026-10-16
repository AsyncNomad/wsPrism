//! Experimental raw transport (ADR 0007 stage 1, `gateway.transport: raw`).
//!
//! Stage 1 tests the send-side hypothesis only: the gateway writes WebSocket
//! frames itself — a per-recipient header (2-10 bytes, from a shared arena)
//! plus the **shared payload `Bytes`**, sent with `writev` — instead of
//! tungstenite copying every payload into its own write buffer. Reading
//! still goes through tungstenite on the socket's read half.
//!
//! Known stage-1 limitations (not for production): tungstenite's automatic
//! replies (pong, close echo) are discarded because its write side is a
//! sink; we send our own close frames. Stage 2 replaces the reader.

use std::io::IoSlice;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self as tg};
use tokio_tungstenite::WebSocketStream;

use super::io::{RecvError, SessionRx, SessionTx};
use crate::realtime::OutFrame;
use crate::transport::codec::{decode_tungstenite, Inbound};
use crate::transport::subprotocol::ProtocolVersion;

/// How long to keep the socket open after closing for an oversized message.
const CLOSE_LINGER: Duration = Duration::from_millis(500);
/// Flush early once this much is queued in one burst.
const MAX_PENDING: usize = 256 * 1024;
/// Most slices handed to one `writev`.
const MAX_IOV: usize = 64;

/// The read half, presented to tungstenite as a full stream whose writes are
/// discarded (stage-1 limitation, see module docs).
pub(crate) struct ReadOnly<R>(R);

impl<R: AsyncRead + Unpin> AsyncRead for ReadOnly<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<R: Unpin> AsyncWrite for ReadOnly<R> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub(crate) struct RawTx<W> {
    w: W,
    protocol: ProtocolVersion,
    /// Header and payload slices of the current burst, in send order.
    slices: Vec<Bytes>,
    pending: usize,
    /// Arena the per-recipient headers are cut from (frozen splits share it).
    hdr: BytesMut,
}

pub(crate) struct RawRx<R> {
    stream: WebSocketStream<ReadOnly<R>>,
    protocol: ProtocolVersion,
}

/// Build the session halves from an upgraded connection's read and write
/// halves. `read_buf` holds bytes the HTTP layer read past the upgrade
/// request (already WebSocket).
pub(crate) async fn from_halves<R, W>(
    r: R,
    w: W,
    read_buf: Bytes,
    protocol: ProtocolVersion,
    max_message_bytes: usize,
) -> (RawTx<W>, RawRx<R>)
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let config = WebSocketConfig {
        max_message_size: Some(max_message_bytes),
        max_frame_size: Some(max_message_bytes),
        ..WebSocketConfig::default()
    };
    let stream = if read_buf.is_empty() {
        WebSocketStream::from_raw_socket(ReadOnly(r), Role::Server, Some(config)).await
    } else {
        WebSocketStream::from_partially_read(
            ReadOnly(r),
            read_buf.to_vec(),
            Role::Server,
            Some(config),
        )
        .await
    };
    let tx = RawTx {
        w,
        protocol,
        slices: Vec::new(),
        pending: 0,
        hdr: BytesMut::new(),
    };
    (tx, RawRx { stream, protocol })
}

impl<W: AsyncWrite + Unpin + Send> RawTx<W> {
    /// Server frame header (FIN set, unmasked): 2, 4 or 10 bytes.
    fn header(&mut self, opcode: u8, len: usize) -> Bytes {
        if self.hdr.capacity() < 10 {
            self.hdr.reserve(1024);
        }
        self.hdr.put_u8(0x80 | opcode);
        if len < 126 {
            self.hdr.put_u8(len as u8);
        } else if len <= u16::MAX as usize {
            self.hdr.put_u8(126);
            self.hdr.put_u16(len as u16);
        } else {
            self.hdr.put_u8(127);
            self.hdr.put_u64(len as u64);
        }
        self.hdr.split().freeze()
    }

    fn push(&mut self, opcode: u8, payload: Bytes) {
        let h = self.header(opcode, payload.len());
        self.pending += h.len() + payload.len();
        self.slices.push(h);
        if !payload.is_empty() {
            self.slices.push(payload);
        }
    }

    /// Write every queued slice with as few `writev` calls as possible.
    async fn write_all_slices(&mut self) -> std::io::Result<()> {
        let mut idx = 0; // first slice not fully written
        let mut off = 0; // bytes of slices[idx] already written
        while idx < self.slices.len() {
            let mut iov = [IoSlice::new(&[]); MAX_IOV];
            let mut n_iov = 0;
            for (i, s) in self.slices[idx..].iter().take(MAX_IOV).enumerate() {
                iov[n_iov] = IoSlice::new(if i == 0 { &s[off..] } else { &s[..] });
                n_iov += 1;
            }
            let mut written = self.w.write_vectored(&iov[..n_iov]).await?;
            if written == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            while written > 0 {
                let left = self.slices[idx].len() - off;
                if written >= left {
                    written -= left;
                    idx += 1;
                    off = 0;
                } else {
                    off += written;
                    written = 0;
                }
            }
        }
        self.slices.clear();
        self.pending = 0;
        Ok(())
    }
}

impl<W: AsyncWrite + Unpin + Send> SessionTx for RawTx<W> {
    async fn send(&mut self, frame: OutFrame) -> Result<(), ()> {
        self.feed(frame).await?;
        self.flush().await
    }

    async fn feed(&mut self, frame: OutFrame) -> Result<(), ()> {
        match frame {
            OutFrame::Text(t) => self.push(0x1, t.into_bytes()),
            OutFrame::Binary(b) => self.push(0x2, b),
            OutFrame::Ping => self.push(0x9, Bytes::new()),
            OutFrame::Hot(h) => {
                let b = match self.protocol {
                    ProtocolVersion::V1 => h.v1.clone(),
                    ProtocolVersion::V2 => h.v2.clone(),
                };
                self.push(0x2, b)
            }
        }
        if self.pending >= MAX_PENDING {
            return self.write_all_slices().await.map_err(|_| ());
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), ()> {
        self.write_all_slices().await.map_err(|_| ())
    }

    async fn send_close(&mut self, code: u16, reason: String) -> Result<(), ()> {
        let mut payload = BytesMut::with_capacity(2 + reason.len().min(123));
        payload.put_u16(code);
        payload.extend_from_slice(&reason.as_bytes()[..reason.len().min(123)]);
        self.push(0x8, payload.freeze());
        self.flush().await
    }

    async fn finish_peer_close(&mut self) {
        // Echo a close (tungstenite's own reply went to the discard sink).
        self.push(0x8, Bytes::new());
        let _ = self.flush().await;
    }

    fn too_big_linger(&self) -> Duration {
        CLOSE_LINGER
    }
}

impl<R: AsyncRead + Unpin + Send> SessionRx for RawRx<R> {
    async fn recv(&mut self) -> Option<Result<Inbound, RecvError>> {
        Some(match self.stream.next().await? {
            Ok(msg) => decode_tungstenite(msg, self.protocol).map_err(RecvError::Decode),
            Err(tg::Error::Capacity(tg::error::CapacityError::MessageTooLong { .. })) => {
                Err(RecvError::TooBig)
            }
            Err(_) => Err(RecvError::Broken),
        })
    }

    async fn await_peer_close(&mut self) {
        while let Some(Ok(m)) = self.stream.next().await {
            if matches!(m, tg::Message::Close(_)) {
                break;
            }
        }
    }
}
