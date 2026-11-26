//! An established, encrypted Nearby Connections link, driven by its own task.
//!
//! The task answers keep-alives and performs the "bandwidth upgrade": the
//! connection starts on whatever medium found the peer (usually Bluetooth)
//! and is then moved to Wi-Fi, either over the shared local network or over
//! a temporary hotspot on a spare Wi-Fi adapter. The receiving side of a
//! connection proposes the upgrade path; the sending side connects to it.
//! Both sides then retire the old channel with LAST_WRITE_TO_PRIOR_CHANNEL /
//! SAFE_TO_CLOSE_PRIOR_CHANNEL and continue on the new one with the same
//! encryption keys and sequence numbers.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::crypto::{Opener, Sealer};
use crate::lan;
use crate::proto::{self, OfflineFrame, UpgradePathInfo, bwu_event, frame_type, medium};
use crate::transport::{self, BoxStream, FrameReader, FrameWriter, RawReader, RawWriter};
use crate::wifi::{Lease, Wifi};

const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the peer gets to connect to a path we offered.
const OFFER_TIMEOUT: Duration = Duration::from_secs(30);
const SWITCH_TIMEOUT: Duration = Duration::from_secs(20);
const INTRO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the peer gets to answer a request to host a path for us.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// Pseudo medium for "ask the peer to host Wi-Fi Direct / a hotspot".
const ASK_PEER: i32 = -1;

pub struct LinkConfig {
    /// Allowed to move to Wi-Fi.
    pub upgrade: bool,
    /// May briefly take over the connected Wi-Fi adapter.
    pub interrupt_ok: bool,
    pub wifi: Option<Arc<Wifi>>,
    /// Medium the connection was established on.
    pub medium: i32,
    /// The peer connected to us (we are receiving): we propose upgrades.
    pub incoming: bool,
    /// Mediums the peer listed in its connection request.
    pub peer_mediums: Vec<i32>,
    pub peer_meta: Option<proto::MediumMetadata>,
    /// Our Wi-Fi abilities; when sending, we ask the receiver to host a path.
    pub local_meta: Option<proto::MediumMetadata>,
    pub local_endpoint_id: String,
}

enum Cmd {
    Frame(Box<OfflineFrame>),
    Close(oneshot::Sender<()>),
}

/// Handle used by a Quick Share session.
pub struct Link {
    frames: mpsc::Receiver<Result<OfflineFrame>>,
    out: mpsc::Sender<Cmd>,
    medium: watch::Receiver<i32>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
}

impl Link {
    pub fn start(rx: FrameReader, tx: FrameWriter, cfg: LinkConfig) -> Result<Link> {
        let (reader, opener) = rx.into_parts();
        let (writer, sealer) = tx.into_parts();
        let opener = opener.ok_or_else(|| anyhow!("link is not encrypted"))?;
        let sealer = sealer.ok_or_else(|| anyhow!("link is not encrypted"))?;
        let (frames_tx, frames) = mpsc::channel(64);
        let (out, out_rx) = mpsc::channel(8);
        let (medium_tx, medium) = watch::channel(cfg.medium);
        let (raw_tx, raw_rx) = mpsc::channel(8);
        let (bwu_tx, bwu_rx) = mpsc::channel(8);
        let reader = spawn_reader(reader, 0, raw_tx.clone());
        let failure = Arc::new(std::sync::Mutex::new(None));
        let request_at = (!cfg.incoming && cfg.upgrade)
            .then_some(())
            .and(cfg.local_meta.as_ref())
            .and_then(|m| m.medium_role.as_ref())
            .filter(|r| r.support_wifi_hotspot_client == Some(true))
            .map(|_| Instant::now() + Duration::from_secs(2));
        let task = Task {
            failure: Arc::clone(&failure),
            request_at,
            cfg,
            writer,
            sealer,
            opener,
            epoch: 0,
            reader,
            raw_tx,
            raw_rx,
            frames: frames_tx,
            out: out_rx,
            medium: medium_tx,
            last_rx: Instant::now(),
            last_ka: Instant::now(),
            paused: false,
            plaintext: false,
            peer_disables_encryption: false,
            replan: None,
            bwu: Bwu::Idle,
            bwu_tx,
            bwu_rx,
            candidates: VecDeque::new(),
            leases: Vec::new(),
            sent_last_write: false,
            peer_last_write: false,
        };
        tokio::spawn(task.run());
        Ok(Link {
            frames,
            out,
            medium,
            failure,
        })
    }

    fn closed(&self) -> anyhow::Error {
        match self.failure.lock().ok().and_then(|f| f.clone()) {
            Some(reason) => anyhow!("{reason}"),
            None => anyhow!("connection closed"),
        }
    }

    /// Next frame for the session (keep-alives and upgrades are handled here).
    pub async fn recv(&mut self) -> Result<OfflineFrame> {
        match self.frames.recv().await {
            Some(r) => r,
            None => Err(self.closed()),
        }
    }

    /// A frame that already arrived, without waiting.
    pub fn try_recv(&mut self) -> Option<Result<OfflineFrame>> {
        match self.frames.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => Some(Err(self.closed())),
        }
    }

    pub async fn send(&self, frame: OfflineFrame) -> Result<()> {
        if self.out.send(Cmd::Frame(Box::new(frame))).await.is_err() {
            return Err(self.closed());
        }
        Ok(())
    }

    /// Wrap a sharing frame into a BYTES payload and send it.
    pub async fn send_sharing(&self, frame: &proto::SharingFrame) -> Result<()> {
        let body = frame.encode_to_vec();
        let id = crate::util::random_payload_id();
        let len = body.len() as i64;
        self.send(proto::payload_chunk(
            id,
            proto::payload_type::BYTES,
            len,
            None,
            0,
            body,
            false,
        ))
        .await?;
        self.send(proto::payload_chunk(
            id,
            proto::payload_type::BYTES,
            len,
            None,
            len,
            Vec::new(),
            true,
        ))
        .await
    }

    /// The medium currently carrying the connection.
    pub fn medium(&self) -> i32 {
        *self.medium.borrow()
    }

    /// Send everything queued, then close the connection.
    pub async fn close(self) {
        let (tx, rx) = oneshot::channel();
        if self.out.send(Cmd::Close(tx)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
    }
}

fn spawn_reader(
    mut r: RawReader,
    epoch: u64,
    tx: mpsc::Sender<(u64, Result<Bytes>)>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let item = match r.next().await {
                Some(Ok(b)) => Ok(b.freeze()),
                Some(Err(e)) => Err(anyhow::Error::from(e).context("connection error")),
                None => Err(anyhow!("connection closed by the other device")),
            };
            let end = item.is_err();
            if tx.send((epoch, item)).await.is_err() || end {
                break;
            }
        }
    })
}

enum Bwu {
    Idle,
    /// We asked the peer to host a path (Wi-Fi Direct) and wait for its offer.
    Requesting {
        until: Instant,
    },
    /// We host a path and wait for the peer to connect to it.
    Offering {
        medium: i32,
        task: JoinHandle<()>,
    },
    /// The peer offered a path and we are connecting to it.
    Connecting {
        task: JoinHandle<()>,
    },
    /// The new channel is up; waiting for the peer to retire the old one.
    Switching {
        stream: BoxStream,
        medium: i32,
        lease: Option<Lease>,
        since: Instant,
    },
    Done,
}

enum BwuEvent {
    /// The path we host is ready: tell the peer about it.
    Offer(UpgradePathInfo),
    /// A new channel is established (introduction exchanged).
    Ready {
        stream: BoxStream,
        medium: i32,
        lease: Option<Lease>,
    },
    HostFailed(anyhow::Error),
    ConnectFailed(UpgradePathInfo, anyhow::Error),
}

struct Task {
    cfg: LinkConfig,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    /// When to ask the receiver to host a Wi-Fi path (we are sending).
    request_at: Option<Instant>,
    writer: RawWriter,
    sealer: Sealer,
    opener: Opener,
    epoch: u64,
    reader: JoinHandle<()>,
    raw_tx: mpsc::Sender<(u64, Result<Bytes>)>,
    raw_rx: mpsc::Receiver<(u64, Result<Bytes>)>,
    frames: mpsc::Sender<Result<OfflineFrame>>,
    out: mpsc::Receiver<Cmd>,
    medium: watch::Sender<i32>,
    last_rx: Instant,
    last_ka: Instant,
    /// After our LAST_WRITE nothing but SAFE_TO_CLOSE may go out on the old channel.
    paused: bool,
    bwu: Bwu,
    bwu_tx: mpsc::Sender<BwuEvent>,
    bwu_rx: mpsc::Receiver<BwuEvent>,
    /// Upgrade mediums still to try (when we propose).
    candidates: VecDeque<(i32, Option<Ipv4Addr>)>,
    /// Hotspots/networks in use by this link.
    leases: Vec<Lease>,
    sent_last_write: bool,
    peer_last_write: bool,
    /// On the upgraded Wi-Fi link both sides drop the extra encryption layer
    /// when the hosting peer offers that (the Wi-Fi link is WPA2-protected).
    plaintext: bool,
    peer_disables_encryption: bool,
    /// New upgrade options from the peer that arrived while we were busy.
    replan: Option<Vec<i32>>,
}

impl Task {
    async fn run(mut self) {
        if let Err(e) = self.plan_upgrade().await {
            debug!("Upgrade: not proposing any ({e:#})");
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let result: Result<()> = loop {
            tokio::select! {
                Some((epoch, raw)) = self.raw_rx.recv() => {
                    if epoch != self.epoch {
                        continue;
                    }
                    let step = match raw {
                        Ok(bytes) => {
                            self.last_rx = Instant::now();
                            self.on_raw(&bytes).await
                        }
                        Err(e) => Err(e),
                    };
                    if let Err(e) = step {
                        break Err(e);
                    }
                }
                cmd = self.out.recv(), if !self.paused => match cmd {
                    Some(Cmd::Frame(frame)) => {
                        if let Err(e) = self.write(&frame).await {
                            break Err(e);
                        }
                    }
                    Some(Cmd::Close(done)) => {
                        let _ = SinkExt::<Bytes>::close(&mut self.writer).await;
                        let _ = done.send(());
                        break Ok(());
                    }
                    None => break Ok(()),
                },
                Some(ev) = self.bwu_rx.recv() => {
                    if let Err(e) = self.on_bwu_event(ev).await {
                        break Err(e);
                    }
                }
                _ = tick.tick() => {
                    if let Err(e) = self.on_tick().await {
                        break Err(e);
                    }
                }
            }
        };
        self.reader.abort();
        match std::mem::replace(&mut self.bwu, Bwu::Done) {
            Bwu::Offering { task, .. } | Bwu::Connecting { task } => task.abort(),
            _ => {}
        }
        if let Err(e) = result {
            debug!("Link ended: {e:#}");
            if let Ok(mut f) = self.failure.lock() {
                *f = Some(format!("{e:#}"));
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), self.frames.send(Err(e))).await;
        }
        // Leases (temporary Wi-Fi) are released when `self` is dropped.
    }

    async fn write(&mut self, frame: &OfflineFrame) -> Result<()> {
        if self.plaintext {
            return self
                .writer
                .send(Bytes::from(frame.encode_to_vec()))
                .await
                .context("failed to send");
        }
        let sealed = self.sealer.seal(&frame.encode_to_vec());
        self.writer
            .send(Bytes::from(sealed))
            .await
            .context("failed to send")
    }

    async fn on_tick(&mut self) -> Result<()> {
        ensure!(
            self.last_rx.elapsed() < IDLE_TIMEOUT,
            "lost connection to the other device"
        );
        if let Bwu::Switching { since, .. } = &self.bwu {
            ensure!(
                since.elapsed() < SWITCH_TIMEOUT,
                "switching to Wi-Fi stalled"
            );
        }
        if self.request_at.is_some_and(|t| Instant::now() >= t) {
            self.request_at = None;
            if matches!(self.bwu, Bwu::Idle) {
                info!("Upgrade: asking the other device to host a Wi-Fi connection");
                self.send_path_request(self.cfg.local_meta.clone()).await?;
            }
        }
        if matches!(self.bwu, Bwu::Idle)
            && let Some(mediums) = self.replan.take()
        {
            info!("Upgrade: trying again with the other device's new options");
            self.cfg.peer_mediums = mediums;
            self.candidates.clear();
            if let Err(e) = self.plan_upgrade().await {
                debug!("Upgrade: still no path ({e:#})");
            }
        }
        if let Bwu::Requesting { until } = self.bwu
            && Instant::now() >= until
        {
            info!("Upgrade: the other device did not offer a Wi-Fi path");
            self.offer_next().await?;
        }
        if !self.paused && self.last_ka.elapsed() >= KEEP_ALIVE_EVERY {
            self.last_ka = Instant::now();
            self.write(&proto::keep_alive(false)).await?;
        }
        Ok(())
    }

    async fn on_raw(&mut self, bytes: &[u8]) -> Result<()> {
        let frame = match self.opener.open(bytes) {
            Ok(plain) => OfflineFrame::decode(plain.as_slice()).context("invalid OfflineFrame")?,
            // Phones stop encrypting once on a WPA2-protected Wi-Fi Direct or
            // hotspot link (whose password went over the encrypted channel).
            // Never accept that on the original channel.
            Err(e) if self.epoch == 0 => return Err(e),
            Err(e) => match OfflineFrame::decode(bytes) {
                Ok(f) if f.v1.is_some() => f,
                _ => {
                    let head: String = bytes.iter().take(48).map(|b| format!("{b:02x}")).collect();
                    debug!("Link: undecodable frame ({} bytes): {head}", bytes.len());
                    return Err(e);
                }
            },
        };
        let v1 = frame.v1.clone().unwrap_or_default();
        match v1.r#type.unwrap_or_default() {
            frame_type::KEEP_ALIVE => {
                let ack = v1.keep_alive.and_then(|k| k.ack).unwrap_or(false);
                if !ack && !self.paused {
                    self.write(&proto::keep_alive(true)).await?;
                }
                Ok(())
            }
            frame_type::BANDWIDTH_UPGRADE_NEGOTIATION => {
                self.on_bwu_frame(v1.bandwidth_upgrade_negotiation.unwrap_or_default())
                    .await
            }
            frame_type::BANDWIDTH_UPGRADE_RETRY => {
                let retry = v1.bandwidth_upgrade_retry.unwrap_or_default();
                let mediums = retry.supported_medium;
                debug!(
                    "Upgrade: peer now supports {:?}",
                    mediums.iter().map(|m| medium::name(*m)).collect::<Vec<_>>()
                );
                // Typically the phone just (re)joined Wi-Fi: try again if we
                // are still on the slow medium and nothing is in progress.
                if self.cfg.incoming && !mediums.is_empty() {
                    match self.bwu {
                        Bwu::Idle => {
                            self.cfg.peer_mediums = mediums;
                            self.candidates.clear();
                            if let Err(e) = self.plan_upgrade().await {
                                debug!("Upgrade: still no path ({e:#})");
                            }
                        }
                        // Use them if the current attempt fails.
                        Bwu::Offering { .. } | Bwu::Requesting { .. } | Bwu::Connecting { .. } => {
                            self.replan = Some(mediums);
                        }
                        Bwu::Switching { .. } | Bwu::Done => {}
                    }
                }
                Ok(())
            }
            _ => {
                // The session may have finished; that is not an error here.
                let _ = self.frames.send(Ok(frame)).await;
                Ok(())
            }
        }
    }

    fn wifi(&self) -> Option<Arc<Wifi>> {
        self.cfg.wifi.clone()
    }

    // ----- proposing an upgrade (we received the connection) -----

    async fn plan_upgrade(&mut self) -> Result<()> {
        if !self.cfg.incoming {
            return Ok(());
        }
        ensure!(self.cfg.upgrade, "disabled in the settings");
        ensure!(self.cfg.medium != medium::WIFI_LAN, "already on Wi-Fi");
        let peer = &self.cfg.peer_mediums;
        let peer_ip = self
            .cfg
            .peer_meta
            .as_ref()
            .and_then(|m| m.ip_address.as_deref())
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(Ipv4Addr::from)
            .filter(|ip| !ip.is_unspecified());
        info!(
            "Upgrade: peer supports {:?} (peer address {})",
            peer.iter().map(|m| medium::name(*m)).collect::<Vec<_>>(),
            peer_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
        if peer.is_empty() || peer.contains(&medium::WIFI_LAN) {
            let addrs = lan::local_addresses();
            let same_net = peer_ip.and_then(|ip| addrs.iter().find(|a| a.contains(ip)));
            match (same_net, peer_ip) {
                (Some(a), _) => self.candidates.push_back((medium::WIFI_LAN, Some(a.ip))),
                // Without a hint, still try the LAN first.
                (None, None) if !addrs.is_empty() => {
                    self.candidates.push_back((medium::WIFI_LAN, None))
                }
                _ => {}
            }
        }
        let (can_join, can_host) = match self.wifi() {
            Some(w) => (w.can_join(self.cfg.interrupt_ok).await, w.can_host().await),
            None => (false, false),
        };
        // Preferred: the phone hosts Wi-Fi Direct and stays on its own Wi-Fi.
        if can_join && (peer.contains(&medium::WIFI_DIRECT) || peer.contains(&medium::WIFI_HOTSPOT))
        {
            self.cfg.local_meta = Some(proto::MediumMetadata {
                supports_5_ghz: Some(true),
                medium_role: Some(proto::MediumRole {
                    support_wifi_direct_group_owner: Some(false),
                    support_wifi_direct_group_client: Some(true),
                    support_wifi_hotspot_host: Some(can_host),
                    support_wifi_hotspot_client: Some(true),
                }),
                ..Default::default()
            });
            self.candidates.push_back((ASK_PEER, None));
        }
        // Fallback: our own hotspot (the phone joins it like a normal network).
        if can_host && peer.contains(&medium::WIFI_HOTSPOT) {
            self.candidates.push_back((medium::WIFI_HOTSPOT, None));
        }
        ensure!(!self.candidates.is_empty(), "no shared Wi-Fi path");
        self.offer_next().await
    }

    async fn offer_next(&mut self) -> Result<()> {
        let Some((m, ip)) = self.candidates.pop_front() else {
            info!(
                "Upgrade: staying on {}",
                medium::name(*self.medium.borrow())
            );
            self.bwu = Bwu::Idle;
            return Ok(());
        };
        if m == ASK_PEER {
            info!("Upgrade: asking the other device to host Wi-Fi Direct");
            self.send_path_request(self.cfg.local_meta.clone()).await?;
            self.bwu = Bwu::Requesting {
                until: Instant::now() + REQUEST_TIMEOUT,
            };
            return Ok(());
        }
        info!("Upgrade: offering {}", medium::name(m));
        // We host and offered to keep encryption on.
        self.peer_disables_encryption = false;
        let events = self.bwu_tx.clone();
        let wifi = self.wifi();
        let task = tokio::spawn(async move {
            if let Err(e) = host_path(m, ip, wifi, events.clone()).await {
                let _ = events.send(BwuEvent::HostFailed(e)).await;
            }
        });
        self.bwu = Bwu::Offering { medium: m, task };
        Ok(())
    }

    /// Ask the peer to host a Wi-Fi Direct group or hotspot that we join.
    async fn send_path_request(&mut self, meta: Option<proto::MediumMetadata>) -> Result<()> {
        self.write(&proto::bwu(bwu_event::UPGRADE_PATH_REQUEST, |b| {
            b.upgrade_path_info = Some(UpgradePathInfo {
                medium: Some(medium::WIFI_DIRECT),
                upgrade_path_request: Some(proto::UpgradePathRequest {
                    mediums: vec![medium::WIFI_DIRECT, medium::WIFI_HOTSPOT],
                    medium_meta_data: meta,
                }),
                ..Default::default()
            })
        }))
        .await
    }

    // ----- frames from the peer -----

    async fn on_bwu_frame(&mut self, b: proto::BandwidthUpgradeNegotiationFrame) -> Result<()> {
        let info = b.upgrade_path_info.clone().unwrap_or_default();
        match b.event_type.unwrap_or_default() {
            bwu_event::UPGRADE_PATH_AVAILABLE => {
                let m = info.medium.unwrap_or_default();
                let allowed = self.cfg.upgrade;
                let expected = match self.bwu {
                    Bwu::Requesting { .. } => true,
                    Bwu::Idle => !self.cfg.incoming,
                    _ => false,
                };
                if !allowed || !expected {
                    debug!("Upgrade: declining {} offered by the peer", medium::name(m));
                    return self.write(&proto::bwu_failure(info)).await;
                }
                info!(
                    "Upgrade: peer offers {} (may disable encryption: {:?}, ack: {:?})",
                    medium::name(m),
                    info.supports_disabling_encryption,
                    info.supports_client_introduction_ack
                );
                self.peer_disables_encryption = info.supports_disabling_encryption == Some(true);
                let events = self.bwu_tx.clone();
                let wifi = self.wifi();
                let me = self.cfg.local_endpoint_id.clone();
                let interrupt = self.cfg.interrupt_ok;
                let task = tokio::spawn(async move {
                    match connect_path(&info, &me, wifi, interrupt).await {
                        Ok((stream, lease)) => {
                            let _ = events
                                .send(BwuEvent::Ready {
                                    stream,
                                    medium: m,
                                    lease,
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = events.send(BwuEvent::ConnectFailed(info, e)).await;
                        }
                    }
                });
                self.bwu = Bwu::Connecting { task };
            }
            bwu_event::UPGRADE_FAILURE => {
                if let Bwu::Offering { medium: m, task } =
                    std::mem::replace(&mut self.bwu, Bwu::Idle)
                {
                    task.abort();
                    info!(
                        "Upgrade: the other device could not use {}",
                        medium::name(m)
                    );
                    self.offer_next().await?;
                }
            }
            bwu_event::LAST_WRITE_TO_PRIOR_CHANNEL => {
                self.peer_last_write = true;
                if self.sent_last_write {
                    self.write(&proto::bwu(bwu_event::SAFE_TO_CLOSE_PRIOR_CHANNEL, |_| {}))
                        .await?;
                }
            }
            bwu_event::SAFE_TO_CLOSE_PRIOR_CHANNEL => self.switch_over(),
            bwu_event::UPGRADE_PATH_REQUEST => {
                // The peer cannot host a path for us and asks us to host one.
                let wanted = info
                    .upgrade_path_request
                    .map(|r| r.mediums)
                    .unwrap_or_default();
                let can_host = match self.wifi() {
                    Some(w) => w.can_host().await,
                    None => false,
                };
                if self.cfg.upgrade
                    && matches!(self.bwu, Bwu::Idle)
                    && can_host
                    && wanted.contains(&medium::WIFI_HOTSPOT)
                {
                    info!("Upgrade: the other device asks us to host a hotspot");
                    self.candidates = VecDeque::from([(medium::WIFI_HOTSPOT, None)]);
                    self.offer_next().await?;
                } else {
                    debug!("Upgrade: cannot host any of {wanted:?} for the other device");
                }
            }
            other => debug!("Upgrade: ignoring negotiation event {other}"),
        }
        Ok(())
    }

    async fn on_bwu_event(&mut self, ev: BwuEvent) -> Result<()> {
        match ev {
            BwuEvent::Offer(info) => {
                if matches!(self.bwu, Bwu::Offering { .. }) {
                    self.write(&proto::bwu(bwu_event::UPGRADE_PATH_AVAILABLE, |b| {
                        b.upgrade_path_info = Some(info)
                    }))
                    .await?;
                }
            }
            BwuEvent::Ready {
                stream,
                medium: m,
                lease,
            } => {
                if !matches!(self.bwu, Bwu::Offering { .. } | Bwu::Connecting { .. }) {
                    return Ok(());
                }
                debug!(
                    "Upgrade: {} channel established, retiring the old channel",
                    medium::name(m)
                );
                self.write(&proto::bwu(bwu_event::LAST_WRITE_TO_PRIOR_CHANNEL, |_| {}))
                    .await?;
                self.sent_last_write = true;
                self.paused = true;
                self.bwu = Bwu::Switching {
                    stream,
                    medium: m,
                    lease,
                    since: Instant::now(),
                };
                if self.peer_last_write {
                    self.write(&proto::bwu(bwu_event::SAFE_TO_CLOSE_PRIOR_CHANNEL, |_| {}))
                        .await?;
                }
            }
            BwuEvent::HostFailed(e) => {
                if let Bwu::Offering { medium: m, .. } = self.bwu {
                    info!("Upgrade: {} unavailable: {e:#}", medium::name(m));
                    self.offer_next().await?;
                }
            }
            BwuEvent::ConnectFailed(info, e) => {
                warn!(
                    "Upgrade: could not use {}: {e:#}",
                    medium::name(info.medium.unwrap_or_default())
                );
                self.bwu = Bwu::Idle;
                self.peer_disables_encryption = false;
                self.write(&proto::bwu_failure(info)).await?;
                if self.cfg.incoming {
                    self.offer_next().await?;
                }
            }
        }
        Ok(())
    }

    /// Both sides are done with the old channel: continue on the new one.
    fn switch_over(&mut self) {
        let Bwu::Switching {
            stream,
            medium: m,
            lease,
            ..
        } = std::mem::replace(&mut self.bwu, Bwu::Done)
        else {
            warn!("Upgrade: unexpected SAFE_TO_CLOSE_PRIOR_CHANNEL");
            return;
        };
        self.reader.abort();
        let (r, w) = tokio::io::split(stream);
        let mut old = std::mem::replace(&mut self.writer, FramedWrite::new(w, transport::codec()));
        // Let our last frames on the old channel drain before closing it.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = SinkExt::<Bytes>::close(&mut old).await;
        });
        self.epoch += 1;
        self.reader = spawn_reader(
            FramedRead::new(r, transport::codec()),
            self.epoch,
            self.raw_tx.clone(),
        );
        self.leases.extend(lease);
        self.paused = false;
        self.plaintext = self.peer_disables_encryption;
        self.sent_last_write = false;
        self.peer_last_write = false;
        self.last_rx = Instant::now();
        self.medium.send_replace(m);
        info!(
            "Upgrade: transfer continues over {}{}",
            medium::name(m),
            if self.plaintext {
                " (protected by the Wi-Fi link encryption)"
            } else {
                ""
            }
        );
    }
}

// ----------------------------------------------------------------------------
// Plain frames on a fresh upgrade socket (before it joins the encrypted link)
// ----------------------------------------------------------------------------

async fn write_plain(s: &mut TcpStream, frame: &OfflineFrame) -> Result<()> {
    let body = frame.encode_to_vec();
    s.write_all(&(body.len() as u32).to_be_bytes()).await?;
    s.write_all(&body).await?;
    Ok(())
}

async fn read_plain(s: &mut TcpStream) -> Result<OfflineFrame> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= 64 * 1024, "unexpected frame size {len}");
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    Ok(OfflineFrame::decode(body.as_slice())?)
}

fn bwu_event_of(frame: &OfflineFrame) -> Option<(i32, proto::BandwidthUpgradeNegotiationFrame)> {
    let b = frame.v1.as_ref()?.bandwidth_upgrade_negotiation.clone()?;
    Some((b.event_type?, b))
}

/// Host an upgrade path, announce it and wait for the peer to connect.
async fn host_path(
    m: i32,
    preferred_ip: Option<Ipv4Addr>,
    wifi: Option<Arc<Wifi>>,
    events: mpsc::Sender<BwuEvent>,
) -> Result<()> {
    let (lease, ips) = match m {
        medium::WIFI_LAN => {
            let mut ips: Vec<Ipv4Addr> = lan::local_addresses().into_iter().map(|a| a.ip).collect();
            // The preferred address goes last: older phones only read `ip_address`.
            if let Some(p) = preferred_ip {
                ips.retain(|ip| *ip != p);
                ips.push(p);
            }
            ensure!(!ips.is_empty(), "no local network address");
            (None, ips)
        }
        medium::WIFI_HOTSPOT => {
            let wifi = wifi.ok_or_else(|| anyhow!("NetworkManager is not available"))?;
            let (lease, hotspot) = wifi.host().await?;
            let ip = lease.local_ip;
            (Some((lease, hotspot)), vec![ip])
        }
        _ => bail!("cannot host {}", medium::name(m)),
    };
    let bind_ip = if m == medium::WIFI_HOTSPOT {
        ips[0]
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    let listener = TcpListener::bind((bind_ip, 0))
        .await
        .context("cannot open a TCP port")?;
    let port = listener.local_addr()?.port();
    let best = *ips.last().expect("non-empty");
    let candidates: Vec<proto::ServiceAddress> = ips
        .iter()
        .map(|ip| proto::ServiceAddress {
            ip_address: Some(ip.octets().to_vec()),
            port: Some(i32::from(port)),
        })
        .collect();
    let mut info = UpgradePathInfo {
        medium: Some(m),
        supports_client_introduction_ack: Some(true),
        supports_disabling_encryption: Some(false),
        ..Default::default()
    };
    let mut lease_out = None;
    match &lease {
        None => {
            info.wifi_lan_socket = Some(proto::WifiLanSocket {
                ip_address: Some(best.octets().to_vec()),
                wifi_port: Some(i32::from(port)),
                address_candidates: candidates,
            });
            debug!("Upgrade: listening on {best}:{port} for the other device");
        }
        Some((_, hs)) => {
            info.wifi_hotspot_credentials = Some(proto::WifiHotspotCredentials {
                ssid: Some(hs.ssid.clone()),
                password: Some(hs.password.clone()),
                port: Some(i32::from(port)),
                gateway: Some(best.to_string()),
                frequency: Some(hs.frequency.map(|f| f as i32).unwrap_or(-1)),
                address_candidates: candidates,
            });
        }
    }
    if let Some((l, _)) = lease {
        lease_out = Some(l);
    }
    events
        .send(BwuEvent::Offer(info))
        .await
        .map_err(|_| anyhow!("link closed"))?;

    let (mut stream, addr) = tokio::time::timeout(OFFER_TIMEOUT, listener.accept())
        .await
        .map_err(|_| anyhow!("the other device did not connect"))??;
    let _ = stream.set_nodelay(true);
    debug!("Upgrade: {addr} connected");
    let intro = tokio::time::timeout(INTRO_TIMEOUT, read_plain(&mut stream))
        .await
        .context("no introduction")??;
    let Some((bwu_event::CLIENT_INTRODUCTION, b)) = bwu_event_of(&intro) else {
        bail!("expected a client introduction");
    };
    debug!(
        "Upgrade: client introduction from endpoint {}",
        b.client_introduction
            .and_then(|c| c.endpoint_id)
            .unwrap_or_default()
    );
    write_plain(
        &mut stream,
        &proto::bwu(bwu_event::CLIENT_INTRODUCTION_ACK, |b| {
            b.client_introduction_ack = Some(Default::default())
        }),
    )
    .await?;
    events
        .send(BwuEvent::Ready {
            stream: Box::new(stream),
            medium: m,
            lease: lease_out,
        })
        .await
        .map_err(|_| anyhow!("link closed"))?;
    Ok(())
}

fn addr_of(a: &proto::ServiceAddress) -> Option<SocketAddr> {
    let ip = <[u8; 4]>::try_from(a.ip_address.as_deref()?).ok()?;
    let port = u16::try_from(a.port?).ok().filter(|p| *p > 0)?;
    Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port)))
}

/// Connect to an upgrade path the peer offered and introduce ourselves.
async fn connect_path(
    info: &UpgradePathInfo,
    me: &str,
    wifi: Option<Arc<Wifi>>,
    allow_interrupt: bool,
) -> Result<(BoxStream, Option<Lease>)> {
    let m = info.medium.unwrap_or_default();
    let mut lease = None;
    let mut addrs: Vec<SocketAddr> = Vec::new();
    match m {
        medium::WIFI_LAN => {
            let s = info
                .wifi_lan_socket
                .clone()
                .ok_or_else(|| anyhow!("no address"))?;
            addrs.extend(s.address_candidates.iter().filter_map(addr_of));
            if let Some(a) = addr_of(&proto::ServiceAddress {
                ip_address: s.ip_address,
                port: s.wifi_port,
            }) && !addrs.contains(&a)
            {
                addrs.push(a);
            }
            let local = lan::local_addresses();
            // Unreachable addresses would only stall; skip foreign subnets.
            addrs.retain(
                |a| matches!(a, SocketAddr::V4(v4) if local.iter().any(|l| l.contains(*v4.ip()))),
            );
            ensure!(
                !addrs.is_empty(),
                "the other device is on a different network"
            );
        }
        medium::WIFI_HOTSPOT | medium::WIFI_DIRECT => {
            let (ssid, password, gateway, port, freq, cands) = if m == medium::WIFI_HOTSPOT {
                let c = info
                    .wifi_hotspot_credentials
                    .clone()
                    .ok_or_else(|| anyhow!("no hotspot credentials"))?;
                (
                    c.ssid,
                    c.password,
                    c.gateway,
                    c.port,
                    c.frequency,
                    c.address_candidates,
                )
            } else {
                let c = info
                    .wifi_direct_credentials
                    .clone()
                    .ok_or_else(|| anyhow!("no Wi-Fi Direct credentials"))?;
                (
                    c.ssid,
                    c.password,
                    c.gateway,
                    c.port,
                    c.frequency,
                    Vec::new(),
                )
            };
            let ssid = ssid
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("no network name"))?;
            let password = password.unwrap_or_default();
            let wifi = wifi.ok_or_else(|| anyhow!("NetworkManager is not available"))?;
            let freq = freq.and_then(|f| u32::try_from(f).ok()).filter(|f| *f > 0);
            lease = Some(wifi.join(&ssid, &password, freq, allow_interrupt).await?);
            addrs.extend(cands.iter().filter_map(addr_of));
            if let (Some(gw), Some(port)) = (gateway.and_then(|g| g.parse::<Ipv4Addr>().ok()), port)
                && let Ok(port) = u16::try_from(port)
            {
                let a = SocketAddr::V4(SocketAddrV4::new(gw, port));
                if !addrs.contains(&a) {
                    addrs.push(a);
                }
            }
            ensure!(!addrs.is_empty(), "no address to connect to");
        }
        other => bail!("{} is not supported", medium::name(other)),
    }

    let mut last_err = anyhow!("no address");
    let mut stream = None;
    // A freshly joined network may need a moment for DHCP/ARP.
    let attempts = if lease.is_some() { 3 } else { 1 };
    'outer: for attempt in 0..attempts {
        for addr in &addrs {
            match tokio::time::timeout(Duration::from_secs(4), TcpStream::connect(addr)).await {
                Ok(Ok(s)) => {
                    stream = Some(s);
                    break 'outer;
                }
                Ok(Err(e)) => last_err = anyhow!("{addr}: {e}"),
                Err(_) => last_err = anyhow!("{addr}: timed out"),
            }
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }
    let mut stream = stream.ok_or(last_err)?;
    let _ = stream.set_nodelay(true);
    write_plain(
        &mut stream,
        &proto::bwu(bwu_event::CLIENT_INTRODUCTION, |b| {
            b.client_introduction = Some(proto::ClientIntroduction {
                endpoint_id: Some(me.to_owned()),
                supports_disabling_encryption: Some(
                    info.supports_disabling_encryption.unwrap_or(false),
                ),
                last_endpoint_id: None,
            })
        }),
    )
    .await?;
    if info.supports_client_introduction_ack.unwrap_or(false) {
        let ack = tokio::time::timeout(INTRO_TIMEOUT, read_plain(&mut stream))
            .await
            .context("no acknowledgement")??;
        ensure!(
            matches!(
                bwu_event_of(&ack),
                Some((bwu_event::CLIENT_INTRODUCTION_ACK, _))
            ),
            "unexpected reply to the introduction"
        );
    }
    Ok((Box::new(stream), lease))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Ukey2Client, Ukey2Server};

    fn data(n: u32) -> OfflineFrame {
        proto::payload_chunk(
            7,
            proto::payload_type::FILE,
            1 << 30,
            Some("f"),
            i64::from(n),
            n.to_be_bytes().to_vec(),
            false,
        )
    }

    fn seq_of(f: &OfflineFrame) -> u32 {
        let body =
            f.v1.clone()
                .unwrap()
                .payload_transfer
                .unwrap()
                .payload_chunk
                .unwrap()
                .body
                .unwrap();
        u32::from_be_bytes(body.try_into().unwrap())
    }

    /// Two links over TCP; the receiving one moves the connection to a
    /// second TCP connection on the local network while data flows both ways.
    #[tokio::test]
    async fn upgrade_to_lan_keeps_order() {
        let Some(local) = lan::local_addresses().into_iter().next() else {
            eprintln!("no network address, skipping");
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (a, b) = tokio::join!(listener.accept(), TcpStream::connect(addr));
        let (rx_a, mut tx_a) = transport::split(Box::new(a.unwrap().0));
        let (rx_b, mut tx_b) = transport::split(Box::new(b.unwrap()));
        let (mut rx_a, mut rx_b) = (rx_a, rx_b);

        let client = Ukey2Client::new();
        let server = Ukey2Server::new(client.client_init()).unwrap();
        let (finish, hs_b) = client.finish(server.server_init()).unwrap();
        let hs_a = server.finish(&finish).unwrap();
        assert_eq!(hs_a.pin, hs_b.pin);
        let (seal_a, open_a) = hs_a.keys.split();
        let (seal_b, open_b) = hs_b.keys.split();
        tx_a.set_sealer(seal_a);
        rx_a.set_opener(open_a);
        tx_b.set_sealer(seal_b);
        rx_b.set_opener(open_b);

        let meta = proto::MediumMetadata {
            ip_address: Some(local.ip.octets().to_vec()),
            ..Default::default()
        };
        let mut receiver = Link::start(
            rx_a,
            tx_a,
            LinkConfig {
                upgrade: true,
                interrupt_ok: false,
                wifi: None,
                medium: medium::BLUETOOTH,
                incoming: true,
                peer_mediums: vec![medium::WIFI_LAN, medium::BLUETOOTH],
                peer_meta: Some(meta),
                local_meta: None,
                local_endpoint_id: "AAAA".into(),
            },
        )
        .unwrap();
        let mut sender = Link::start(
            rx_b,
            tx_b,
            LinkConfig {
                upgrade: true,
                interrupt_ok: false,
                wifi: None,
                medium: medium::BLUETOOTH,
                incoming: false,
                peer_mediums: Vec::new(),
                peer_meta: None,
                local_meta: None,
                local_endpoint_id: "BBBB".into(),
            },
        )
        .unwrap();

        let total = 400u32;
        let send_task = tokio::spawn(async move {
            for n in 0..total {
                sender.send(data(n)).await.unwrap();
                if n % 50 == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            // Wait for the receiver's reply, which arrives over the new channel.
            let reply = tokio::time::timeout(Duration::from_secs(10), sender.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(seq_of(&reply), 4242);
            let m = sender.medium();
            sender.close().await;
            m
        });
        for n in 0..total {
            let f = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(seq_of(&f), n, "frames out of order");
        }
        assert_eq!(receiver.medium(), medium::WIFI_LAN);
        receiver.send(data(4242)).await.unwrap();
        assert_eq!(send_task.await.unwrap(), medium::WIFI_LAN);
        receiver.close().await;
    }
}
