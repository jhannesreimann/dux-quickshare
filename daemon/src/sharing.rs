//! Quick Share sessions on top of an established stream (TCP or RFCOMM):
//! Nearby Connections handshake, then the sharing protocol for receiving
//! and sending files, text and links.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use log::{debug, info, warn};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::crypto::{Ukey2Client, Ukey2Server, random_vec};
use crate::daemon::{Daemon, Direction, Item, Transfer, TransferState};
use crate::endpoint::{self, DeviceType, LocalEndpoint};
use crate::link::{Link, LinkConfig};
use crate::proto::{
    self, OfflineFrame, SharingFrame, SharingV1Frame, V1Frame, frame_type, payload_type,
    response_status, sharing_type, text_type,
};
use crate::transport::{self, BoxStream, FrameReader, FrameWriter};
use crate::util;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const CONSENT_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Marker error: the local user cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

// ----------------------------------------------------------------------------
// Frame helpers
// ----------------------------------------------------------------------------

fn sharing_frame(ty: i32, f: impl FnOnce(&mut SharingV1Frame)) -> SharingFrame {
    let mut v1 = SharingV1Frame {
        r#type: Some(ty),
        ..Default::default()
    };
    f(&mut v1);
    proto::sharing(v1)
}

fn paired_key_encryption() -> SharingFrame {
    sharing_frame(sharing_type::PAIRED_KEY_ENCRYPTION, |v| {
        v.paired_key_encryption = Some(proto::PairedKeyEncryptionFrame {
            signed_data: Some(random_vec(72)),
            secret_id_hash: Some(random_vec(6)),
        })
    })
}

fn paired_key_result() -> SharingFrame {
    sharing_frame(sharing_type::PAIRED_KEY_RESULT, |v| {
        v.paired_key_result = Some(proto::PairedKeyResultFrame {
            status: Some(proto::PAIRED_KEY_RESULT_UNABLE),
        })
    })
}

fn response(status: i32) -> SharingFrame {
    sharing_frame(sharing_type::RESPONSE, |v| {
        v.connection_response = Some(proto::SharingResponseFrame {
            status: Some(status),
        })
    })
}

fn cancel_frame() -> SharingFrame {
    sharing_frame(sharing_type::CANCEL, |_| {})
}

/// Reassembles BYTES payloads (sharing frames and text) from chunks.
#[derive(Default)]
struct BytesPayloads(HashMap<i64, Vec<u8>>);

impl BytesPayloads {
    fn push(&mut self, id: i64, body: &[u8], last: bool) -> Result<Option<Vec<u8>>> {
        let buf = self.0.entry(id).or_default();
        ensure!(
            buf.len() + body.len() <= 4 * 1024 * 1024,
            "text payload too large"
        );
        buf.extend_from_slice(body);
        Ok(if last { self.0.remove(&id) } else { None })
    }
}

/// Things that happen on the connection that sessions react to.
enum Incoming {
    Disconnected,
    Sharing(SharingV1Frame),
    /// A chunk of a non-sharing payload.
    Chunk {
        id: i64,
        kind: i32,
        offset: i64,
        body: Vec<u8>,
        last: bool,
    },
    Other,
}

/// Interpret an OfflineFrame (keep-alives and upgrades are handled by the link).
fn classify(
    frame: OfflineFrame,
    sharing_ids: &mut BytesPayloads,
    data_ids: &dyn Fn(i64) -> bool,
) -> Result<Incoming> {
    let v1 = frame.v1.unwrap_or_default();
    Ok(match v1.r#type.unwrap_or_default() {
        frame_type::DISCONNECTION => Incoming::Disconnected,
        frame_type::PAYLOAD_TRANSFER => {
            let pt = v1.payload_transfer.unwrap_or_default();
            if pt.packet_type == Some(proto::PACKET_TYPE_CONTROL) {
                return Ok(Incoming::Other);
            }
            let header = pt.payload_header.unwrap_or_default();
            let chunk = pt.payload_chunk.unwrap_or_default();
            let id = header.id.unwrap_or_default();
            let kind = header.r#type.unwrap_or_default();
            let last = chunk.flags.unwrap_or(0) & proto::CHUNK_FLAG_LAST != 0;
            let body = chunk.body.unwrap_or_default();
            if kind == payload_type::BYTES && !data_ids(id) {
                match sharing_ids.push(id, &body, last)? {
                    Some(bytes) => {
                        let frame = SharingFrame::decode(bytes.as_slice())
                            .context("invalid sharing frame")?;
                        match frame.v1 {
                            Some(v1) => Incoming::Sharing(v1),
                            None => Incoming::Other,
                        }
                    }
                    None => Incoming::Other,
                }
            } else {
                Incoming::Chunk {
                    id,
                    kind,
                    offset: chunk.offset.unwrap_or_default(),
                    body,
                    last,
                }
            }
        }
        _ => Incoming::Other,
    })
}

// ----------------------------------------------------------------------------
// Nearby Connections handshake
// ----------------------------------------------------------------------------

fn expect_connection_response(frame: OfflineFrame) -> Result<()> {
    let v1 = frame.v1.ok_or_else(|| anyhow!("empty frame"))?;
    ensure!(
        v1.r#type == Some(frame_type::CONNECTION_RESPONSE),
        "expected a connection response"
    );
    let resp = v1.connection_response.unwrap_or_default();
    ensure!(
        resp.response == Some(proto::CONNECTION_RESPONSE_ACCEPT),
        "the other device refused the connection"
    );
    Ok(())
}

/// Responder side. Returns the peer's connection request and the PIN.
async fn server_handshake(
    rx: &mut FrameReader,
    tx: &mut FrameWriter,
) -> Result<(proto::ConnectionRequestFrame, String)> {
    let first = rx.recv_plain_frame(HANDSHAKE_TIMEOUT).await?;
    let v1 = first.v1.ok_or_else(|| anyhow!("empty frame"))?;
    ensure!(
        v1.r#type == Some(frame_type::CONNECTION_REQUEST),
        "expected a connection request"
    );
    let request = v1.connection_request.unwrap_or_default();

    let client_init = rx.recv_raw_timeout(HANDSHAKE_TIMEOUT).await?;
    let server = Ukey2Server::new(&client_init)?;
    tx.send_raw(server.server_init().to_vec()).await?;
    let client_finish = rx.recv_raw_timeout(HANDSHAKE_TIMEOUT).await?;
    let hs = server.finish(&client_finish)?;

    tx.send_plain(&proto::connection_response_accept()).await?;
    expect_connection_response(rx.recv_plain_frame(HANDSHAKE_TIMEOUT).await?)?;
    let (sealer, opener) = hs.keys.split();
    tx.set_sealer(sealer);
    rx.set_opener(opener);
    Ok((request, hs.pin))
}

/// Initiator side. Returns the PIN.
async fn client_handshake(
    rx: &mut FrameReader,
    tx: &mut FrameWriter,
    local: &LocalEndpoint,
    mediums: Vec<i32>,
    medium_metadata: Option<proto::MediumMetadata>,
) -> Result<String> {
    let request = proto::offline(V1Frame {
        r#type: Some(frame_type::CONNECTION_REQUEST),
        connection_request: Some(proto::ConnectionRequestFrame {
            endpoint_id: Some(local.endpoint_id.clone()),
            endpoint_name: Some(local.name.clone()),
            endpoint_info: Some(local.endpoint_info()),
            mediums,
            medium_metadata,
            nonce: Some(rand::random::<i32>()),
            ..Default::default()
        }),
        ..Default::default()
    });
    tx.send_plain(&request).await?;
    let client = Ukey2Client::new();
    tx.send_raw(client.client_init().to_vec()).await?;
    let server_init = rx.recv_raw_timeout(HANDSHAKE_TIMEOUT).await?;
    let (client_finish, hs) = client.finish(&server_init)?;
    tx.send_raw(client_finish).await?;
    tx.send_plain(&proto::connection_response_accept()).await?;
    expect_connection_response(rx.recv_plain_frame(HANDSHAKE_TIMEOUT).await?)?;
    let (sealer, opener) = hs.keys.split();
    tx.set_sealer(sealer);
    rx.set_opener(opener);
    Ok(hs.pin)
}

fn medium_code(medium: &str) -> i32 {
    match medium {
        "bluetooth" => proto::medium::BLUETOOTH,
        "ble" => proto::medium::BLE,
        _ => proto::medium::WIFI_LAN,
    }
}

/// Mediums we can move a connection to and what our Wi-Fi can do (sent
/// when we connect, so the receiving phone can offer a Wi-Fi path).
async fn upgrade_mediums(d: &Daemon, current: i32) -> (Vec<i32>, Option<proto::MediumMetadata>) {
    let mut out = Vec::new();
    let mut meta = None;
    if d.config().wifi_upgrade {
        let addrs = crate::lan::local_addresses();
        if !addrs.is_empty() {
            out.push(proto::medium::WIFI_LAN);
        }
        let (can_join, can_host) = match d.wifi() {
            Some(wifi) => (
                wifi.can_join(d.config().wifi_interrupt).await,
                wifi.can_host().await,
            ),
            None => (false, false),
        };
        if can_join || can_host {
            out.push(proto::medium::WIFI_DIRECT);
            out.push(proto::medium::WIFI_HOTSPOT);
        }
        // Phones only start a Wi-Fi upgrade towards clients. A laptop that
        // can only host still says so: it declines the phone's own offer at
        // once, and the phone then asks it to host instead (role switch).
        let client = can_join || can_host;
        meta = Some(proto::MediumMetadata {
            supports_5_ghz: Some(true),
            ip_address: addrs.first().map(|a| a.ip.octets().to_vec()),
            medium_role: Some(proto::MediumRole {
                support_wifi_direct_group_owner: Some(false),
                support_wifi_direct_group_client: Some(client),
                support_wifi_hotspot_host: Some(can_host),
                support_wifi_hotspot_client: Some(client),
            }),
            ..Default::default()
        });
    }
    if !out.contains(&current) {
        out.push(current);
    }
    (out, meta)
}

// ----------------------------------------------------------------------------
// Receiving
// ----------------------------------------------------------------------------

struct IncomingFile {
    name: String,
    path: PathBuf,
    size: u64,
    file: Option<tokio::fs::File>,
    written: u64,
    done: bool,
}

struct IncomingText {
    kind: i32,
    wifi_ssid: Option<String>,
    done: bool,
}

struct Inbound {
    d: Arc<Daemon>,
    medium: &'static str,
    tid: Option<String>,
    cancel: CancellationToken,
    files: HashMap<i64, IncomingFile>,
    texts: HashMap<i64, IncomingText>,
    text_bytes: BytesPayloads,
    received_text: Vec<String>,
    bytes_done: u64,
    last_progress: Instant,
}

pub async fn run_inbound(d: Arc<Daemon>, stream: BoxStream, medium: &'static str) {
    let mut session = Inbound {
        d,
        medium,
        tid: None,
        cancel: CancellationToken::new(),
        files: HashMap::new(),
        texts: HashMap::new(),
        text_bytes: BytesPayloads::default(),
        received_text: Vec::new(),
        bytes_done: 0,
        last_progress: Instant::now(),
    };
    let result = session.run(stream).await;
    session.finish(result).await;
}

fn free_space(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs fills a plain struct for a valid C string path.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut st) == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
    }
}

impl Inbound {
    fn all_done(&self) -> bool {
        self.files.values().all(|f| f.done) && self.texts.values().all(|t| t.done)
    }

    async fn run(&mut self, stream: BoxStream) -> Result<TransferState> {
        let (mut rx, mut tx) = transport::split(stream);
        let (request, pin) = server_handshake(&mut rx, &mut tx).await?;
        let mut link = Link::start(
            rx,
            tx,
            LinkConfig {
                upgrade: self.d.config().wifi_upgrade,
                interrupt_ok: self.d.config().wifi_interrupt,
                wifi: self.d.wifi(),
                medium: medium_code(self.medium),
                incoming: true,
                peer_mediums: request.mediums.clone(),
                peer_meta: request.medium_metadata.clone(),
                local_meta: None,
                local_endpoint_id: self.d.local().endpoint_id,
            },
        )?;
        let result = self.serve(&mut link, request, pin).await;
        link.close().await;
        result
    }

    async fn serve(
        &mut self,
        link: &mut Link,
        request: proto::ConnectionRequestFrame,
        pin: String,
    ) -> Result<TransferState> {
        let info = request
            .endpoint_info
            .as_deref()
            .and_then(endpoint::parse_endpoint_info);
        let peer_name = info
            .as_ref()
            .and_then(|i| i.name.clone())
            .or(request.endpoint_name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Unknown device".to_owned());
        let peer_type = info.map(|i| i.device_type).unwrap_or(DeviceType::Unknown);
        info!("Connected to {peer_name} over {} (PIN {pin})", self.medium);

        link.send_sharing(&paired_key_encryption()).await?;
        let mut sharing_bytes = BytesPayloads::default();
        let mut consent: Option<oneshot::Receiver<bool>> = None;
        let mut consent_deadline = tokio::time::Instant::now();

        loop {
            let ids: Vec<i64> = self
                .files
                .keys()
                .chain(self.texts.keys())
                .copied()
                .collect();
            tokio::select! {
                frame = link.recv() => {
                    let frame = frame?;
                    self.medium = proto::medium::name(link.medium());
                    match classify(frame, &mut sharing_bytes, &|id| ids.contains(&id))? {
                        Incoming::Disconnected => {
                            if self.tid.is_some() && (!self.files.is_empty() || !self.texts.is_empty()) && self.all_done() {
                                return Ok(TransferState::Completed);
                            }
                            if self.tid.is_none() {
                                bail!("{peer_name} disconnected");
                            }
                            return Ok(TransferState::Cancelled);
                        }
                        Incoming::Sharing(v1) => match v1.r#type.unwrap_or_default() {
                            sharing_type::PAIRED_KEY_ENCRYPTION => link.send_sharing(&paired_key_result()).await?,
                            sharing_type::PAIRED_KEY_RESULT => {}
                            sharing_type::INTRODUCTION => {
                                let intro = v1.introduction.unwrap_or_default();
                                let auto = self.introduce(&intro, &peer_name, peer_type, &pin)?;
                                if auto {
                                    if let Some(state) = self.accept(link).await? {
                                        return Ok(state);
                                    }
                                } else if let Some(tid) = &self.tid {
                                    consent = Some(self.d.request_consent(tid));
                                    consent_deadline = tokio::time::Instant::now() + CONSENT_TIMEOUT;
                                }
                            }
                            sharing_type::CANCEL => {
                                info!("{peer_name} cancelled the transfer");
                                return Ok(TransferState::Cancelled);
                            }
                            _ => {}
                        },
                        Incoming::Chunk { id, kind, offset, body, last } => {
                            if self.on_chunk(id, kind, offset, &body, last).await? {
                                link.send(proto::disconnection()).await.ok();
                                return Ok(TransferState::Completed);
                            }
                        }
                        Incoming::Other => {}
                    }
                }
                decision = async { consent.as_mut().expect("guarded").await }, if consent.is_some() => {
                    consent = None;
                    if decision.unwrap_or(false) {
                        if let Some(state) = self.accept(link).await? {
                            return Ok(state);
                        }
                    } else {
                        link.send_sharing(&response(response_status::REJECT)).await?;
                        link.send(proto::disconnection()).await.ok();
                        return Ok(TransferState::Rejected);
                    }
                }
                _ = tokio::time::sleep_until(consent_deadline), if consent.is_some() => {
                    link.send_sharing(&response(response_status::TIMED_OUT)).await.ok();
                    link.send(proto::disconnection()).await.ok();
                    bail!("the request was not answered in time");
                }
                _ = self.cancel.cancelled() => {
                    link.send_sharing(&cancel_frame()).await.ok();
                    link.send(proto::disconnection()).await.ok();
                    return Err(Cancelled.into());
                }
            }
        }
    }

    /// Register the announced items. Returns true if they should be accepted
    /// without asking.
    fn introduce(
        &mut self,
        intro: &proto::IntroductionFrame,
        peer: &str,
        peer_type: DeviceType,
        pin: &str,
    ) -> Result<bool> {
        let mut items = Vec::new();
        for f in &intro.file_metadata {
            let name = util::sanitize_file_name(f.name.as_deref().unwrap_or("file"));
            let size = f.size.unwrap_or(0).max(0) as u64;
            let mime = f
                .mime_type
                .clone()
                .unwrap_or_else(|| util::mime_for(&name).0.to_owned());
            items.push(Item {
                name: name.clone(),
                size,
                kind: "file".into(),
                mime,
            });
            self.files.insert(
                f.payload_id.unwrap_or_default(),
                IncomingFile {
                    name,
                    path: PathBuf::new(),
                    size,
                    file: None,
                    written: 0,
                    done: false,
                },
            );
        }
        for t in &intro.text_metadata {
            let kind = t.r#type.unwrap_or(text_type::TEXT);
            items.push(Item {
                name: t.text_title.clone().unwrap_or_default(),
                size: t.size.unwrap_or(0).max(0) as u64,
                kind: util::text_kind_name(kind).into(),
                mime: "text/plain".into(),
            });
            self.texts.insert(
                t.payload_id.unwrap_or_default(),
                IncomingText {
                    kind,
                    wifi_ssid: None,
                    done: false,
                },
            );
        }
        for w in &intro.wifi_credentials_metadata {
            let ssid = w.ssid.clone().unwrap_or_default();
            items.push(Item {
                name: ssid.clone(),
                size: 0,
                kind: "wifi".into(),
                mime: String::new(),
            });
            self.texts.insert(
                w.payload_id.unwrap_or_default(),
                IncomingText {
                    kind: -1,
                    wifi_ssid: Some(ssid),
                    done: false,
                },
            );
        }
        ensure!(
            !items.is_empty(),
            "{peer} offered nothing that can be received"
        );

        let mut t = Transfer::new(Direction::Incoming, peer, peer_type, self.medium);
        t.state = TransferState::AwaitingConsent;
        t.pin = Some(pin.to_owned());
        t.total_bytes = items.iter().map(|i| i.size).sum();
        t.items = items;
        let summary = t.summary();
        let (tid, cancel) = self.d.add_transfer(t);
        info!("{peer} wants to send {summary} ({tid})");
        self.tid = Some(tid);
        self.cancel = cancel;
        Ok(self.d.config().auto_accept)
    }

    /// Accept: create the target files and tell the sender to go ahead.
    /// Returns a final state if the transfer cannot proceed.
    async fn accept(&mut self, link: &Link) -> Result<Option<TransferState>> {
        let dir = self.d.download_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let needed: u64 = self.files.values().map(|f| f.size).sum();
        if free_space(&dir).is_some_and(|free| free < needed.saturating_add(16 * 1024 * 1024)) {
            link.send_sharing(&response(response_status::NOT_ENOUGH_SPACE))
                .await?;
            bail!("not enough free space in {}", dir.display());
        }
        let mut saved = Vec::new();
        for f in self.files.values_mut() {
            let name = f.name.clone();
            // create_new reserves the name, so files in one transfer never collide.
            let mut attempt = 0;
            let file = loop {
                let path = util::unique_path(&dir, &name);
                match tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .await
                {
                    Ok(file) => {
                        f.path = path;
                        break file;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 50 => {
                        attempt += 1
                    }
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("cannot create a file in {}", dir.display()));
                    }
                }
            };
            f.file = Some(file);
            saved.push(f.path.to_string_lossy().into_owned());
        }
        link.send_sharing(&response(response_status::ACCEPT))
            .await?;
        let tid = self.tid.clone().unwrap_or_default();
        self.d.update_transfer(&tid, |t| {
            t.state = TransferState::Transferring;
            t.saved_paths = saved;
        });
        Ok(None)
    }

    /// Handle a data chunk. Returns true once everything has been received.
    async fn on_chunk(
        &mut self,
        id: i64,
        kind: i32,
        offset: i64,
        body: &[u8],
        last: bool,
    ) -> Result<bool> {
        let tid = self
            .tid
            .clone()
            .ok_or_else(|| anyhow!("data before introduction"))?;
        if let Some(f) = self.files.get_mut(&id) {
            ensure!(
                kind == payload_type::FILE || kind == payload_type::BYTES,
                "unexpected payload type {kind}"
            );
            let file = f
                .file
                .as_mut()
                .ok_or_else(|| anyhow!("data for a file that was not accepted"))?;
            if offset as u64 != f.written {
                use tokio::io::AsyncSeekExt;
                file.seek(std::io::SeekFrom::Start(offset.max(0) as u64))
                    .await?;
                f.written = offset.max(0) as u64;
            }
            if !body.is_empty() {
                file.write_all(body)
                    .await
                    .with_context(|| format!("cannot write {}", f.path.display()))?;
                f.written += body.len() as u64;
                self.bytes_done += body.len() as u64;
            }
            if last {
                file.flush().await?;
                f.file = None;
                f.done = true;
                if f.size > 0 && f.written != f.size {
                    warn!(
                        "{}: received {} of {} bytes",
                        f.path.display(),
                        f.written,
                        f.size
                    );
                }
            }
        } else if let Some(t) = self.texts.get_mut(&id) {
            if let Some(bytes) = self.text_bytes.push(id, body, last)? {
                t.done = true;
                self.bytes_done += bytes.len() as u64;
                let text = if let Some(ssid) = &t.wifi_ssid {
                    let creds =
                        proto::WifiCredentials::decode(bytes.as_slice()).unwrap_or_default();
                    format!(
                        "Wi-Fi network: {ssid}\nPassword: {}",
                        creds.password.unwrap_or_default()
                    )
                } else {
                    let _ = t.kind;
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                self.received_text.push(text);
            }
        } else {
            debug!("Ignoring chunk of unknown payload {id}");
            return Ok(false);
        }

        let done = self.all_done();
        if done || last || self.last_progress.elapsed() > Duration::from_millis(250) {
            self.last_progress = Instant::now();
            let bytes = self.bytes_done;
            let medium = self.medium;
            self.d.update_transfer(&tid, |t| {
                t.bytes_done = bytes.min(t.total_bytes.max(bytes));
                t.medium = medium.to_owned();
            });
        }
        Ok(done)
    }

    async fn finish(mut self, result: Result<TransferState>) {
        let Some(tid) = self.tid.clone() else {
            if let Err(e) = result {
                debug!("Incoming connection ended: {e:#}");
            }
            return;
        };
        // Close files and remove the ones that did not arrive completely.
        let mut kept = Vec::new();
        for f in self.files.values_mut() {
            f.file = None;
            if f.done {
                kept.push(f.path.to_string_lossy().into_owned());
            } else if !f.path.as_os_str().is_empty() {
                let _ = tokio::fs::remove_file(&f.path).await;
            }
        }
        let text = (!self.received_text.is_empty()).then(|| self.received_text.join("\n\n"));
        self.d.update_transfer(&tid, |t| {
            t.saved_paths = kept;
            t.text = text.clone();
        });
        match result {
            Ok(TransferState::Completed) => {
                if text.is_some()
                    && let Some(t) = self.d.transfer(&tid)
                {
                    self.d.emit_text_received(&t);
                }
                self.d.finish_transfer(&tid, TransferState::Completed, None);
            }
            Ok(state) => self.d.finish_transfer(&tid, state, None),
            Err(e) if e.is::<Cancelled>() => {
                self.d.finish_transfer(&tid, TransferState::Cancelled, None)
            }
            Err(e) => self
                .d
                .finish_transfer(&tid, TransferState::Failed, Some(format!("{e:#}"))),
        }
    }
}

// ----------------------------------------------------------------------------
// Sending
// ----------------------------------------------------------------------------

pub enum Outgoing {
    Files(Vec<PathBuf>),
    Text(String),
}

fn text_preview(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        out.push('…');
    }
    out
}

impl Outgoing {
    pub fn items(&self) -> Result<Vec<Item>> {
        match self {
            Outgoing::Files(paths) => paths
                .iter()
                .map(|p| {
                    let meta = std::fs::metadata(p)
                        .with_context(|| format!("cannot read {}", p.display()))?;
                    ensure!(meta.is_file(), "{} is not a regular file", p.display());
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into());
                    let mime = util::mime_for(&name).0.to_owned();
                    Ok(Item {
                        name,
                        size: meta.len(),
                        kind: "file".into(),
                        mime,
                    })
                })
                .collect(),
            Outgoing::Text(text) => Ok(vec![Item {
                name: text_preview(text),
                size: text.len() as u64,
                kind: if util::looks_like_url(text) {
                    "url"
                } else {
                    "text"
                }
                .into(),
                mime: "text/plain".into(),
            }]),
        }
    }
}

enum OutItem {
    File {
        id: i64,
        path: PathBuf,
        name: String,
        size: u64,
    },
    Text {
        id: i64,
        bytes: Vec<u8>,
    },
}

fn build_introduction(payload: &Outgoing) -> Result<(proto::IntroductionFrame, Vec<OutItem>)> {
    let mut intro = proto::IntroductionFrame::default();
    let mut out = Vec::new();
    match payload {
        Outgoing::Files(paths) => {
            for p in paths {
                let size = std::fs::metadata(p)
                    .with_context(|| format!("cannot read {}", p.display()))?
                    .len();
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                let (mime, kind) = util::mime_for(&name);
                let id = util::random_payload_id();
                intro.file_metadata.push(proto::FileMetadata {
                    name: Some(name.clone()),
                    r#type: Some(kind),
                    payload_id: Some(id),
                    size: Some(size as i64),
                    mime_type: Some(mime.to_owned()),
                    id: Some(util::random_payload_id()),
                    parent_folder: None,
                });
                out.push(OutItem::File {
                    id,
                    path: p.clone(),
                    name,
                    size,
                });
            }
        }
        Outgoing::Text(text) => {
            let id = util::random_payload_id();
            let kind = if util::looks_like_url(text) {
                text_type::URL
            } else {
                text_type::TEXT
            };
            intro.text_metadata.push(proto::TextMetadata {
                text_title: Some(text_preview(text)),
                r#type: Some(kind),
                payload_id: Some(id),
                size: Some(text.len() as i64),
                id: Some(util::random_payload_id()),
            });
            out.push(OutItem::Text {
                id,
                bytes: text.as_bytes().to_vec(),
            });
        }
    }
    Ok((intro, out))
}

/// What the receiver did while we were streaming data.
fn drain(link: &mut Link, sharing: &mut BytesPayloads) -> Result<Option<TransferState>> {
    while let Some(frame) = link.try_recv() {
        match classify(frame?, sharing, &|_| false)? {
            Incoming::Disconnected => bail!("the other device closed the connection"),
            Incoming::Sharing(v1) if v1.r#type == Some(sharing_type::CANCEL) => {
                return Ok(Some(TransferState::Cancelled));
            }
            _ => {}
        }
    }
    Ok(None)
}

pub async fn run_outbound(
    d: Arc<Daemon>,
    tid: String,
    stream: BoxStream,
    payload: Outgoing,
    cancel: CancellationToken,
) -> Result<TransferState> {
    let medium = d.transfer(&tid).map(|t| t.medium).unwrap_or_default();
    let peer = d.transfer(&tid).map(|t| t.peer_name).unwrap_or_default();
    let local = d.local();
    let (mut rx, mut tx) = transport::split(stream);
    let (mediums, meta) = upgrade_mediums(&d, medium_code(&medium)).await;
    let pin = client_handshake(&mut rx, &mut tx, &local, mediums, meta.clone()).await?;
    info!("Connected to {peer} over {medium} (PIN {pin})");
    d.update_transfer(&tid, |t| t.pin = Some(pin.clone()));
    let mut link = Link::start(
        rx,
        tx,
        LinkConfig {
            upgrade: d.config().wifi_upgrade,
            interrupt_ok: d.config().wifi_interrupt,
            wifi: d.wifi(),
            medium: medium_code(&medium),
            incoming: false,
            peer_mediums: Vec::new(),
            peer_meta: None,
            local_meta: meta,
            local_endpoint_id: local.endpoint_id.clone(),
        },
    )?;
    let result = send_items(&d, &tid, &peer, &mut link, payload, cancel).await;
    if result.is_ok() {
        link.send(proto::disconnection()).await.ok();
    }
    link.close().await;
    result
}

async fn send_items(
    d: &Arc<Daemon>,
    tid: &str,
    peer: &str,
    link: &mut Link,
    payload: Outgoing,
    cancel: CancellationToken,
) -> Result<TransferState> {
    let (intro, items) = build_introduction(&payload)?;
    link.send_sharing(&paired_key_encryption()).await?;
    let mut sharing = BytesPayloads::default();
    let mut deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;

    // Wait for the receiver to accept.
    loop {
        tokio::select! {
            frame = link.recv() => {
                match classify(frame?, &mut sharing, &|_| false)? {
                    Incoming::Disconnected => bail!("{peer} did not accept (the request was declined or timed out)"),
                    Incoming::Sharing(v1) => match v1.r#type.unwrap_or_default() {
                        sharing_type::PAIRED_KEY_ENCRYPTION => link.send_sharing(&paired_key_result()).await?,
                        sharing_type::PAIRED_KEY_RESULT => {
                            link.send_sharing(&sharing_frame(sharing_type::INTRODUCTION, |v| v.introduction = Some(intro.clone()))).await?;
                            d.update_transfer(tid, |t| t.state = TransferState::AwaitingAcceptance);
                            deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
                        }
                        sharing_type::RESPONSE => {
                            match v1.connection_response.and_then(|r| r.status).unwrap_or_default() {
                                response_status::ACCEPT => break,
                                response_status::REJECT => return Ok(TransferState::Rejected),
                                response_status::NOT_ENOUGH_SPACE => bail!("{peer} does not have enough free space"),
                                response_status::UNSUPPORTED_ATTACHMENT_TYPE => bail!("{peer} cannot receive this type of content"),
                                response_status::TIMED_OUT => bail!("{peer} did not accept in time"),
                                other => bail!("{peer} declined (status {other})"),
                            }
                        }
                        sharing_type::CANCEL => return Ok(TransferState::Cancelled),
                        _ => {}
                    },
                    _ => {}
                }
            }
            _ = cancel.cancelled() => {
                link.send_sharing(&cancel_frame()).await.ok();
                link.send(proto::disconnection()).await.ok();
                return Err(Cancelled.into());
            }
            _ = tokio::time::sleep_until(deadline) => bail!("{peer} did not respond"),
        }
    }

    info!("{peer} accepted, sending");
    d.update_transfer(tid, |t| t.state = TransferState::Transferring);
    let mut sent: u64 = 0;
    let mut last_progress = Instant::now();
    let mut buf = Vec::new();
    for item in items {
        match item {
            OutItem::File {
                id,
                path,
                name,
                size,
            } => {
                let mut file = tokio::fs::File::open(&path)
                    .await
                    .with_context(|| format!("cannot open {}", path.display()))?;
                let mut offset: u64 = 0;
                loop {
                    // Small chunks on Bluetooth, large ones once on Wi-Fi.
                    let m = link.medium();
                    let chunk = if m == proto::medium::BLUETOOTH || m == proto::medium::BLE {
                        64 * 1024
                    } else {
                        512 * 1024
                    };
                    buf.resize(chunk, 0);
                    let n = file
                        .read(&mut buf)
                        .await
                        .with_context(|| format!("cannot read {}", path.display()))?;
                    if n == 0 {
                        break;
                    }
                    link.send(proto::payload_chunk(
                        id,
                        payload_type::FILE,
                        size as i64,
                        Some(&name),
                        offset as i64,
                        buf[..n].to_vec(),
                        false,
                    ))
                    .await?;
                    offset += n as u64;
                    sent += n as u64;
                    if let Some(state) = drain(link, &mut sharing)? {
                        return Ok(state);
                    }
                    if cancel.is_cancelled() {
                        link.send_sharing(&cancel_frame()).await.ok();
                        link.send(proto::disconnection()).await.ok();
                        return Err(Cancelled.into());
                    }
                    if last_progress.elapsed() > Duration::from_millis(250) {
                        last_progress = Instant::now();
                        let medium = proto::medium::name(link.medium());
                        d.update_transfer(tid, |t| {
                            t.bytes_done = sent;
                            t.medium = medium.to_owned();
                        });
                    }
                }
                ensure!(offset == size, "{} changed while sending", path.display());
                link.send(proto::payload_chunk(
                    id,
                    payload_type::FILE,
                    size as i64,
                    Some(&name),
                    offset as i64,
                    Vec::new(),
                    true,
                ))
                .await?;
            }
            OutItem::Text { id, bytes } => {
                let len = bytes.len() as i64;
                sent += bytes.len() as u64;
                link.send(proto::payload_chunk(
                    id,
                    payload_type::BYTES,
                    len,
                    None,
                    0,
                    bytes,
                    false,
                ))
                .await?;
                link.send(proto::payload_chunk(
                    id,
                    payload_type::BYTES,
                    len,
                    None,
                    len,
                    Vec::new(),
                    true,
                ))
                .await?;
            }
        }
    }
    d.update_transfer(tid, |t| t.bytes_done = sent);

    // The receiver disconnects once it has stored everything.
    let wait = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(wait);
    loop {
        tokio::select! {
            frame = link.recv() => match frame {
                Ok(frame) => match classify(frame, &mut sharing, &|_| false) {
                    Ok(Incoming::Disconnected) | Err(_) => break,
                    Ok(Incoming::Sharing(v1)) if v1.r#type == Some(sharing_type::CANCEL) => return Ok(TransferState::Cancelled),
                    _ => {}
                },
                Err(_) => break,
            },
            _ = &mut wait => break,
        }
    }
    Ok(TransferState::Completed)
}
