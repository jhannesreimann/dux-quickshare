//! Incoming Quick Share connections over a BLE L2CAP channel.
//!
//! Phones connect to our LE L2CAP PSM (advertised in the BLE header) without
//! pairing, unlike Bluetooth Classic RFCOMM which always triggers a pairing
//! prompt on the phone. The channel carries Nearby's "BLE socket" framing:
//! every packet is prefixed with a 4-byte big-endian length; first come
//! L2CAP command packets (advertisement requests, data connection request),
//! then data packets prefixed with the 3-byte service ID hash and control
//! packets prefixed with three zero bytes. Each data packet is acknowledged.
//! This module turns that into a plain byte stream for the session code.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use log::{debug, info};
use prost::Message;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use crate::endpoint::SERVICE_ID_HASH;
use crate::transport::{BoxStream, set_nonblocking};

const CMD_REQUEST_ADVERTISEMENT: u8 = 1;
const CMD_REQUEST_ADVERTISEMENT_FINISH: u8 = 2;
const CMD_REQUEST_DATA_CONNECTION: u8 = 3;
const CMD_RESPONSE_ADVERTISEMENT: u8 = 21;
const CMD_RESPONSE_DATA_CONNECTION_READY: u8 = 23;

const CONTROL_PREFIX: [u8; 3] = [0, 0, 0];
const MAX_FRAME: usize = 5 * 1024 * 1024;
/// Payload bytes per outgoing packet (well below any CoC MTU).
const MAX_PACKET_DATA: usize = 4000;

#[derive(Clone, PartialEq, Message)]
struct SocketControlFrame {
    #[prost(int32, optional, tag = "1")]
    r#type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    introduction: Option<IntroductionFrame>,
    #[prost(message, optional, tag = "3")]
    disconnection: Option<DisconnectionFrame>,
    #[prost(message, optional, tag = "4")]
    packet_acknowledgement: Option<PacketAcknowledgementFrame>,
}

#[derive(Clone, PartialEq, Message)]
struct IntroductionFrame {
    #[prost(bytes = "vec", optional, tag = "1")]
    service_id_hash: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    socket_version: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct DisconnectionFrame {
    #[prost(bytes = "vec", optional, tag = "1")]
    service_id_hash: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct PacketAcknowledgementFrame {
    #[prost(bytes = "vec", optional, tag = "1")]
    service_id_hash: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    received_size: Option<i32>,
}

const CONTROL_DISCONNECTION: i32 = 2;
const CONTROL_PACKET_ACKNOWLEDGEMENT: i32 = 3;

/// An accepted L2CAP socket (SOCK_SEQPACKET) driven by tokio.
struct Socket {
    fd: AsyncFd<OwnedFd>,
    /// Received bytes not yet split into frames.
    pending: Vec<u8>,
}

impl Socket {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(&fd)?;
        // SAFETY: the OwnedFd stays valid inside the AsyncFd.
        let fd = unsafe { AsyncFd::register(fd) }?;
        Ok(Self {
            fd,
            pending: Vec::new(),
        })
    }

    async fn recv_sdu(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; 65536];
        loop {
            let mut guard = self.fd.readable().await?;
            let res = guard.try_io(|inner| {
                // SAFETY: reading into our own buffer.
                let n =
                    unsafe { libc::recv(inner.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(Ok(n)) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Ok(Err(e)) if e.raw_os_error() == Some(libc::ECONNRESET) => {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    /// Next length-prefixed frame.
    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        loop {
            if self.pending.len() >= 4 {
                let len =
                    u32::from_be_bytes(self.pending[..4].try_into().expect("4 bytes")) as usize;
                ensure!(len <= MAX_FRAME, "BLE frame too large ({len} bytes)");
                if self.pending.len() >= 4 + len {
                    let frame = self.pending[4..4 + len].to_vec();
                    self.pending.drain(..4 + len);
                    return Ok(frame);
                }
            }
            let sdu = self.recv_sdu().await?;
            self.pending.extend_from_slice(&sdu);
        }
    }

    async fn send_frame(&self, data: &[u8]) -> io::Result<()> {
        let mut packet = Vec::with_capacity(4 + data.len());
        packet.extend_from_slice(&(data.len() as u32).to_be_bytes());
        packet.extend_from_slice(data);
        loop {
            let mut guard = self.fd.writable().await?;
            let res = guard.try_io(|inner| {
                // SAFETY: writing from our own buffer; MSG_NOSIGNAL avoids SIGPIPE.
                let n = unsafe {
                    libc::send(
                        inner.as_raw_fd(),
                        packet.as_ptr().cast(),
                        packet.len(),
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
                Ok(Ok(n)) if n == packet.len() => return Ok(()),
                Ok(Ok(n)) => {
                    return Err(io::Error::other(format!(
                        "short write ({n} of {} bytes)",
                        packet.len()
                    )));
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    async fn send_control(&self, frame: &SocketControlFrame) -> io::Result<()> {
        let mut data = CONTROL_PREFIX.to_vec();
        data.extend_from_slice(&frame.encode_to_vec());
        self.send_frame(&data).await
    }
}

/// Run the L2CAP command phase. Returns once the phone asked for a data
/// connection (Ok(true)) or was only fetching advertisements (Ok(false)).
async fn command_phase(sock: &mut Socket) -> Result<bool> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), sock.recv_frame())
            .await
            .map_err(|_| anyhow!("no request from the phone"))??;
        match frame.first().copied() {
            Some(CMD_REQUEST_ADVERTISEMENT) => {
                // Like Nearby on iOS: answer with an empty advertisement
                // (command + zero length) so the phone reads it over GATT.
                sock.send_frame(&[CMD_RESPONSE_ADVERTISEMENT, 0, 0]).await?;
            }
            Some(CMD_REQUEST_ADVERTISEMENT_FINISH) => return Ok(false),
            Some(CMD_REQUEST_DATA_CONNECTION) => {
                sock.send_frame(&[CMD_RESPONSE_DATA_CONNECTION_READY])
                    .await?;
                return Ok(true);
            }
            other => bail!("unexpected L2CAP command {other:?}"),
        }
    }
}

/// Pump bytes between the phone and the session's end of a duplex pipe.
async fn pump(mut sock: Socket, mut local: DuplexStream) -> Result<()> {
    let mut out = vec![0u8; MAX_PACKET_DATA];
    loop {
        tokio::select! {
            frame = sock.recv_frame() => {
                let frame = frame?;
                if frame.len() < 3 {
                    continue;
                }
                if frame[..3] == CONTROL_PREFIX {
                    let control = SocketControlFrame::decode(&frame[3..]).unwrap_or_default();
                    match control.r#type.unwrap_or_default() {
                        CONTROL_DISCONNECTION => {
                            debug!("BLE L2CAP: the phone closed the channel");
                            return Ok(());
                        }
                        CONTROL_PACKET_ACKNOWLEDGEMENT => {}
                        other => debug!("BLE L2CAP: control frame {other}"),
                    }
                } else if frame[..3] == SERVICE_ID_HASH {
                    local.write_all(&frame[3..]).await?;
                    let ack = SocketControlFrame {
                        r#type: Some(CONTROL_PACKET_ACKNOWLEDGEMENT),
                        packet_acknowledgement: Some(PacketAcknowledgementFrame {
                            service_id_hash: Some(SERVICE_ID_HASH.to_vec()),
                            received_size: Some(frame.len() as i32),
                        }),
                        ..Default::default()
                    };
                    sock.send_control(&ack).await?;
                } else if frame.len() == 1 {
                    // A late L2CAP command; nothing to do.
                } else {
                    debug!("BLE L2CAP: ignoring packet for another service");
                }
            }
            n = local.read(&mut out) => {
                let n = n?;
                if n == 0 {
                    let bye = SocketControlFrame {
                        r#type: Some(CONTROL_DISCONNECTION),
                        disconnection: Some(DisconnectionFrame { service_id_hash: Some(SERVICE_ID_HASH.to_vec()) }),
                        ..Default::default()
                    };
                    let _ = sock.send_control(&bye).await;
                    // Give the phone a moment to read everything.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    return Ok(());
                }
                let mut packet = SERVICE_ID_HASH.to_vec();
                packet.extend_from_slice(&out[..n]);
                sock.send_frame(&packet).await?;
            }
        }
    }
}

/// Handle a freshly accepted L2CAP connection. Returns a byte stream when the
/// phone wants to talk Quick Share, None if it only fetched advertisements.
pub async fn accept(fd: OwnedFd) -> Result<Option<BoxStream>> {
    let mut sock = Socket::new(fd)?;
    if !command_phase(&mut sock).await? {
        return Ok(None);
    }
    info!("BLE: incoming Quick Share connection (no pairing needed)");
    let (session_end, pump_end) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        if let Err(e) = pump(sock, pump_end).await {
            debug!("BLE L2CAP: channel ended: {e:#}");
        }
    });
    Ok(Some(Box::new(session_end)))
}
