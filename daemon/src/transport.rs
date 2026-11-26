//! Length-prefixed framing, the encrypted Nearby channel, and an async wrapper
//! for Bluetooth (RFCOMM) sockets.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::crypto::{Opener, Sealer};
use crate::proto::OfflineFrame;

pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}
pub type BoxStream = Box<dyn Stream>;

/// Largest frame we accept (payload chunks are at most a few hundred KiB).
const MAX_FRAME: usize = 8 * 1024 * 1024;

pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .big_endian()
        .max_frame_length(MAX_FRAME)
        .new_codec()
}

pub type RawReader = FramedRead<ReadHalf<BoxStream>, LengthDelimitedCodec>;
pub type RawWriter = FramedWrite<WriteHalf<BoxStream>, LengthDelimitedCodec>;

pub struct FrameReader {
    inner: RawReader,
    opener: Option<Opener>,
}

pub struct FrameWriter {
    inner: RawWriter,
    sealer: Option<Sealer>,
}

pub fn split(stream: BoxStream) -> (FrameReader, FrameWriter) {
    let (r, w) = tokio::io::split(stream);
    (
        FrameReader {
            inner: FramedRead::new(r, codec()),
            opener: None,
        },
        FrameWriter {
            inner: FramedWrite::new(w, codec()),
            sealer: None,
        },
    )
}

impl FrameReader {
    /// Read one raw (unencrypted) frame. Cancel safe.
    pub async fn recv_raw(&mut self) -> Result<Vec<u8>> {
        match self.inner.next().await {
            Some(Ok(b)) => Ok(b.to_vec()),
            Some(Err(e)) => Err(e).context("connection error"),
            None => Err(anyhow!("connection closed by peer")),
        }
    }

    pub async fn recv_raw_timeout(&mut self, t: Duration) -> Result<Vec<u8>> {
        tokio::time::timeout(t, self.recv_raw())
            .await
            .map_err(|_| anyhow!("timed out waiting for the other device"))?
    }

    pub async fn recv_plain_frame(&mut self, t: Duration) -> Result<OfflineFrame> {
        let raw = self.recv_raw_timeout(t).await?;
        OfflineFrame::decode(raw.as_slice()).context("invalid OfflineFrame")
    }

    pub fn set_opener(&mut self, opener: Opener) {
        self.opener = Some(opener);
    }

    pub fn into_parts(self) -> (RawReader, Option<Opener>) {
        (self.inner, self.opener)
    }
}

impl FrameWriter {
    pub async fn send_raw(&mut self, data: Vec<u8>) -> Result<()> {
        self.inner
            .send(Bytes::from(data))
            .await
            .context("failed to send")
    }

    pub async fn send_plain(&mut self, frame: &OfflineFrame) -> Result<()> {
        self.send_raw(frame.encode_to_vec()).await
    }

    pub fn set_sealer(&mut self, sealer: Sealer) {
        self.sealer = Some(sealer);
    }

    pub fn into_parts(self) -> (RawWriter, Option<Sealer>) {
        (self.inner, self.sealer)
    }
}

// ----------------------------------------------------------------------------
// Bluetooth sockets
// ----------------------------------------------------------------------------

/// A connected stream socket (RFCOMM) driven by tokio.
pub struct BtStream {
    fd: AsyncFd<OwnedFd>,
}

impl BtStream {
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(&fd)?;
        // SAFETY: `OwnedFd` owns a valid, open descriptor and returns the same
        // raw fd for as long as it lives inside the `AsyncFd`.
        let fd = unsafe { AsyncFd::register(fd) }?;
        Ok(Self { fd })
    }
}

pub fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a valid, owned descriptor.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl AsyncRead for BtStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            let res = guard.try_io(|inner| {
                // SAFETY: reading into an initialized, correctly sized buffer.
                let n = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        unfilled.as_mut_ptr().cast(),
                        unfilled.len(),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.raw_os_error() == Some(libc::ECONNRESET) => {
                    // Treat a reset after the peer finished as end of stream.
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for BtStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            let res = guard.try_io(|inner| {
                // SAFETY: writing from a valid slice; MSG_NOSIGNAL avoids SIGPIPE.
                let n = unsafe {
                    libc::send(
                        inner.as_raw_fd(),
                        data.as_ptr().cast(),
                        data.len(),
                        libc::MSG_NOSIGNAL,
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(r) => return Poll::Ready(r),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // SAFETY: shutdown on a valid socket.
        unsafe { libc::shutdown(self.fd.get_ref().as_raw_fd(), libc::SHUT_WR) };
        Poll::Ready(Ok(()))
    }
}
