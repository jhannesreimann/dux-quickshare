//! The long-running service: owns the configuration, the device list and the
//! transfers, drives the radios and serves the JSON control socket used by
//! the shell plugin and the CLI.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::bluetooth::{Bluetooth, BtEvent, BtStatus};
use crate::endpoint::{DeviceType, LocalEndpoint, RemoteInfo};
use crate::lan::{self, Lan, LanEvent};
use crate::notify::Notifier;
use crate::sharing::{self, Outgoing};
use crate::transport::{BoxStream, BtStream};
use crate::util;
use crate::wifi::Wifi;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_HISTORY: usize = 30;
const BLE_DEVICE_TTL: Duration = Duration::from_secs(15);
const LAN_DEVICE_TTL: Duration = Duration::from_secs(600);

// ----------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Visible to everyone nearby.
    Everyone,
    /// Visible to everyone for a limited time, then hidden.
    Temporary,
    /// Not visible; sending still works.
    Hidden,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub device_name: String,
    pub visibility: Visibility,
    /// Empty = the XDG download directory.
    pub download_dir: String,
    /// Accept incoming transfers without asking (not recommended).
    pub auto_accept: bool,
    pub use_bluetooth: bool,
    pub use_wifi: bool,
    /// Power the Bluetooth adapter on at startup when visible (never lifts airplane mode).
    pub auto_enable_bluetooth: bool,
    pub notifications: bool,
    pub temporary_minutes: u32,
    /// TCP port for Wi-Fi transfers (0 = random).
    pub port: u16,
    /// Move Bluetooth connections to Wi-Fi for speed: over the shared
    /// network, or a temporary hotspot on an idle Wi-Fi adapter.
    pub wifi_upgrade: bool,
    /// On laptops with one Wi-Fi card that cannot run a hotspot next to the
    /// connection: briefly leave the current network for fast transfers.
    pub wifi_interrupt: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: util::default_device_name(),
            visibility: Visibility::Everyone,
            download_dir: String::new(),
            auto_accept: false,
            use_bluetooth: true,
            use_wifi: true,
            auto_enable_bluetooth: true,
            notifications: true,
            temporary_minutes: 10,
            port: 0,
            wifi_upgrade: true,
            wifi_interrupt: false,
        }
    }
}

fn config_file() -> PathBuf {
    util::config_dir().join("config.json")
}

fn load_config() -> Config {
    match std::fs::read_to_string(config_file()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            warn!(
                "Config: {} is invalid ({e}), using defaults",
                config_file().display()
            );
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn save_config(cfg: &Config) {
    let path = config_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    let result = serde_json::to_vec_pretty(cfg)
        .map_err(anyhow::Error::from)
        .and_then(|data| std::fs::write(&tmp, data).map_err(Into::into))
        .and_then(|()| std::fs::rename(&tmp, &path).map_err(Into::into));
    if let Err(e) = result {
        warn!("Config: cannot save {}: {e}", path.display());
    }
}

// ----------------------------------------------------------------------------
// Devices and transfers
// ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub device_type: DeviceType,
    pub bt: Option<([u8; 6], Instant)>,
    pub lan: Option<(SocketAddr, Instant)>,
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Connecting,
    /// Incoming: waiting for the local user to accept.
    AwaitingConsent,
    /// Outgoing: waiting for the other device to accept.
    AwaitingAcceptance,
    Transferring,
    Completed,
    Rejected,
    Cancelled,
    Failed,
}

impl TransferState {
    pub fn finished(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Rejected | Self::Cancelled | Self::Failed
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub name: String,
    pub size: u64,
    /// "file", "text", "url", "wifi", ...
    pub kind: String,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: String,
    pub direction: Direction,
    pub peer_name: String,
    pub peer_type: DeviceType,
    pub medium: String,
    pub state: TransferState,
    pub pin: Option<String>,
    pub items: Vec<Item>,
    pub total_bytes: u64,
    pub bytes_done: u64,
    pub error: Option<String>,
    pub saved_paths: Vec<String>,
    /// Received text (links, notes) or shared Wi-Fi credentials.
    pub text: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Transfer {
    pub fn new(direction: Direction, peer_name: &str, peer_type: DeviceType, medium: &str) -> Self {
        let now = util::now_ms();
        Self {
            id: String::new(),
            direction,
            peer_name: peer_name.to_owned(),
            peer_type,
            medium: medium.to_owned(),
            state: TransferState::Connecting,
            pin: None,
            items: Vec::new(),
            total_bytes: 0,
            bytes_done: 0,
            error: None,
            saved_paths: Vec::new(),
            text: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Short human description, e.g. "2 photos" or "report.pdf".
    pub fn summary(&self) -> String {
        match self.items.as_slice() {
            [] => "nothing".to_owned(),
            [one] if one.kind == "file" => one.name.clone(),
            [one] if one.kind == "url" => "a link".to_owned(),
            [one] if one.kind == "wifi" => format!("Wi-Fi network \"{}\"", one.name),
            [_] => "a text".to_owned(),
            many => {
                let images = many.iter().filter(|i| i.mime.starts_with("image/")).count();
                let videos = many.iter().filter(|i| i.mime.starts_with("video/")).count();
                if images == many.len() {
                    format!("{} photos", many.len())
                } else if videos == many.len() {
                    format!("{} videos", many.len())
                } else {
                    format!("{} files", many.len())
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// State
// ----------------------------------------------------------------------------

struct State {
    config: Config,
    local: LocalEndpoint,
    devices: BTreeMap<String, Device>,
    transfers: Vec<Transfer>,
    consent: HashMap<String, oneshot::Sender<bool>>,
    cancel: HashMap<String, CancellationToken>,
    visible_until: Option<u64>,
    discovery_until: Option<Instant>,
    ui_clients: usize,
    next_id: u64,
    /// Phones we are sending to over Bluetooth Classic, whose "Just Works"
    /// pairing request we answer (see dux.rs): address -> until when. One
    /// request each.
    pairing_allowed: HashMap<String, Instant>,
    /// Pairings we accepted for Quick Share (device path -> when), removed later.
    temp_pairings: HashMap<String, Instant>,
    /// Device paths with a running incoming Bluetooth session.
    bt_sessions: HashMap<String, usize>,
    recent_pairing_accepts: Vec<Instant>,
}

impl State {
    fn visible(&self) -> bool {
        match self.config.visibility {
            Visibility::Everyone => true,
            Visibility::Temporary => self.visible_until.is_some_and(|t| t > util::now_ms()),
            Visibility::Hidden => false,
        }
    }
}

pub struct Daemon {
    state: Mutex<State>,
    changed: Notify,
    events: broadcast::Sender<Arc<str>>,
    bt: OnceLock<Arc<Bluetooth>>,
    lan: OnceLock<Arc<Lan>>,
    wifi: OnceLock<Arc<Wifi>>,
    notifier: OnceLock<Arc<Notifier>>,
    shutdown: CancellationToken,
}

impl Daemon {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn mark_changed(&self) {
        self.changed.notify_one();
    }

    fn emit(&self, event: Value) {
        let _ = self.events.send(Arc::from(event.to_string()));
    }

    pub fn local(&self) -> LocalEndpoint {
        self.lock().local.clone()
    }

    pub fn config(&self) -> Config {
        self.lock().config.clone()
    }

    /// Temporary Wi-Fi links via NetworkManager, if available.
    pub fn wifi(&self) -> Option<Arc<Wifi>> {
        self.wifi.get().cloned()
    }

    pub fn download_dir(&self) -> PathBuf {
        let dir = self.lock().config.download_dir.clone();
        if dir.trim().is_empty() {
            util::default_download_dir()
        } else {
            util::expand_tilde(dir.trim())
        }
    }

    // ----- transfers -----

    pub fn add_transfer(&self, mut t: Transfer) -> (String, CancellationToken) {
        let mut st = self.lock();
        st.next_id += 1;
        t.id = format!("t{}", st.next_id);
        let id = t.id.clone();
        let token = self.shutdown.child_token();
        st.cancel.insert(id.clone(), token.clone());
        st.transfers.push(t);
        // Drop the oldest finished transfers.
        while st.transfers.len() > MAX_HISTORY {
            match st.transfers.iter().position(|t| t.state.finished()) {
                Some(i) => {
                    st.transfers.remove(i);
                }
                None => break,
            }
        }
        drop(st);
        self.mark_changed();
        (id, token)
    }

    pub fn update_transfer(&self, id: &str, f: impl FnOnce(&mut Transfer)) {
        let mut st = self.lock();
        if let Some(t) = st.transfers.iter_mut().find(|t| t.id == id) {
            f(t);
            t.updated_at = util::now_ms();
        }
        drop(st);
        self.mark_changed();
    }

    pub fn transfer(&self, id: &str) -> Option<Transfer> {
        self.lock().transfers.iter().find(|t| t.id == id).cloned()
    }

    /// Mark a transfer as finished and tell the user.
    pub fn finish_transfer(
        self: &Arc<Self>,
        id: &str,
        state: TransferState,
        error: Option<String>,
    ) {
        let mut st = self.lock();
        st.cancel.remove(id);
        st.consent.remove(id);
        let Some(t) = st.transfers.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if t.state.finished() {
            return;
        }
        t.state = state;
        t.error = error;
        t.updated_at = util::now_ms();
        if state == TransferState::Completed {
            t.bytes_done = t.total_bytes;
        }
        let t = t.clone();
        let notify = st.config.notifications;
        drop(st);
        self.mark_changed();
        match state {
            TransferState::Failed => warn!(
                "Transfer {id} failed: {}",
                t.error.as_deref().unwrap_or("unknown error")
            ),
            _ => info!("Transfer {id} finished: {state:?}"),
        }
        self.emit(json!({ "event": "transferFinished", "transfer": t }));
        if notify && let Some(n) = self.notifier.get().cloned() {
            tokio::spawn(async move { n.transfer_finished(&t).await });
        }
    }

    /// Ask the user to accept an incoming transfer. Resolves to the decision.
    pub fn request_consent(self: &Arc<Self>, id: &str) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        let mut st = self.lock();
        st.consent.insert(id.to_owned(), tx);
        let notify = st.config.notifications;
        let t = st.transfers.iter().find(|t| t.id == id).cloned();
        drop(st);
        self.mark_changed();
        if let Some(t) = t {
            self.emit(json!({ "event": "incomingRequest", "transfer": t }));
            if notify && let Some(n) = self.notifier.get().cloned() {
                tokio::spawn(async move { n.incoming_request(&t).await });
            }
        }
        rx
    }

    pub fn decide(&self, id: &str, accept: bool) -> Result<()> {
        let tx = self
            .lock()
            .consent
            .remove(id)
            .ok_or_else(|| anyhow!("no pending request {id}"))?;
        let _ = tx.send(accept);
        if let Some(n) = self.notifier.get().cloned() {
            let id = id.to_owned();
            tokio::spawn(async move { n.close_for_transfer(&id).await });
        }
        Ok(())
    }

    pub fn cancel_transfer(&self, id: &str) -> Result<()> {
        let st = self.lock();
        let token = st
            .cancel
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("transfer {id} is not running"))?;
        drop(st);
        token.cancel();
        Ok(())
    }

    pub fn emit_text_received(&self, t: &Transfer) {
        self.emit(json!({ "event": "textReceived", "transfer": t }));
    }

    // ----- devices -----

    fn device_seen(&self, id: String, info: RemoteInfo, f: impl FnOnce(&mut Device)) {
        let mut st = self.lock();
        if id == st.local.endpoint_id || !info.visible {
            return;
        }
        let dev = st.devices.entry(id.clone()).or_insert_with(|| Device {
            id,
            name: String::new(),
            device_type: info.device_type,
            bt: None,
            lan: None,
            rssi: None,
        });
        let new = dev.name.is_empty();
        if let Some(name) = info.name {
            dev.name = name;
        }
        dev.device_type = info.device_type;
        f(dev);
        if new {
            debug!("Discovered {} ({})", dev.name, dev.id);
        }
        drop(st);
        self.mark_changed();
    }

    // ----- radios -----

    /// Push the current visibility/discovery wishes to Bluetooth and Wi-Fi.
    pub fn apply_radios(&self) {
        let st = self.lock();
        let visible = st.visible();
        let discovering = st.discovery_until.is_some();
        let local = st.local.clone();
        let cfg = st.config.clone();
        drop(st);
        if let Some(lan) = self.lan.get() {
            lan.set_visible(if visible && cfg.use_wifi {
                Some(&local)
            } else {
                None
            });
            lan.set_browsing(discovering && cfg.use_wifi, local.endpoint_id.clone());
        }
        if let Some(bt) = self.bt.get() {
            bt.set_desired(|d| {
                d.visible = (visible && cfg.use_bluetooth).then(|| local.clone());
                d.scan = discovering && cfg.use_bluetooth;
                d.fast_init = discovering && cfg.use_bluetooth;
            });
        }
        self.mark_changed();
    }

    /// The user chose to be visible or to send: power the adapter on if
    /// needed. Never lifts an rfkill block (airplane mode); only the explicit
    /// "Turn on" action (enableBluetooth) does that.
    fn user_wants_bluetooth(&self) {
        if self.lock().config.use_bluetooth
            && let Some(bt) = self.bt.get()
        {
            bt.request_enable(false);
        }
    }

    pub fn set_visibility(&self, v: Visibility, explicit: bool) {
        let mut st = self.lock();
        st.config.visibility = v;
        st.visible_until = (v == Visibility::Temporary)
            .then(|| util::now_ms() + u64::from(st.config.temporary_minutes.max(1)) * 60_000);
        save_config(&st.config);
        drop(st);
        info!("Visibility set to {v:?}");
        if explicit && v != Visibility::Hidden {
            self.user_wants_bluetooth();
        }
        self.apply_radios();
    }

    pub fn start_discovery(&self, seconds: u64) {
        let mut st = self.lock();
        st.discovery_until = Some(Instant::now() + Duration::from_secs(seconds.clamp(5, 600)));
        drop(st);
        // Looking around never switches Bluetooth on; the panel offers that.
        self.apply_radios();
    }

    pub fn stop_discovery(&self) {
        self.lock().discovery_until = None;
        self.apply_radios();
    }

    fn apply_config(&self, patch: Value, explicit: bool) -> Result<Config> {
        let mut st = self.lock();
        let mut merged = serde_json::to_value(&st.config)?;
        let (Value::Object(base), Value::Object(patch)) = (&mut merged, patch) else {
            bail!("config must be an object");
        };
        for (k, v) in patch {
            base.insert(k, v);
        }
        let mut cfg: Config = serde_json::from_value(merged).context("invalid config value")?;
        cfg.device_name = crate::endpoint::truncate_name(&cfg.device_name);
        if cfg.device_name.is_empty() {
            cfg.device_name = util::default_device_name();
        }
        cfg.temporary_minutes = cfg.temporary_minutes.clamp(1, 24 * 60);
        let old = std::mem::replace(&mut st.config, cfg.clone());
        if old.device_name != cfg.device_name {
            st.local.set_name(&cfg.device_name);
            info!("Device name set to {:?}", cfg.device_name);
        }
        if old.visibility != cfg.visibility {
            st.visible_until = (cfg.visibility == Visibility::Temporary)
                .then(|| util::now_ms() + u64::from(cfg.temporary_minutes) * 60_000);
        }
        save_config(&cfg);
        drop(st);
        if explicit && cfg.visibility != Visibility::Hidden && old.visibility != cfg.visibility {
            self.user_wants_bluetooth();
        }
        self.apply_radios();
        Ok(cfg)
    }

    // ----- sending -----

    pub fn send(self: &Arc<Self>, device_id: &str, payload: Outgoing) -> Result<String> {
        let dev = {
            let st = self.lock();
            // Address by endpoint id, or by (case-insensitive) name.
            st.devices
                .get(device_id)
                .or_else(|| {
                    st.devices
                        .values()
                        .find(|d| d.name.eq_ignore_ascii_case(device_id))
                })
                .cloned()
        }
        .ok_or_else(|| anyhow!("device {device_id} is not nearby anymore"))?;
        let items = payload.items()?;
        let medium = if dev.lan.is_some() {
            "wifi"
        } else {
            "bluetooth"
        };
        let mut t = Transfer::new(Direction::Outgoing, &dev.name, dev.device_type, medium);
        t.total_bytes = items.iter().map(|i| i.size).sum();
        t.items = items;
        let (id, cancel) = self.add_transfer(t);
        info!(
            "Sending {} to {} ({})",
            self.transfer(&id).map(|t| t.summary()).unwrap_or_default(),
            dev.name,
            dev.id
        );
        self.user_wants_bluetooth();
        let d = Arc::clone(self);
        let tid = id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                r = d.connect_and_send(&tid, &dev, payload, cancel.clone()) => r,
                _ = cancel.cancelled() => Err(sharing::Cancelled.into()),
            };
            match result {
                Ok(state) => d.finish_transfer(&tid, state, None),
                Err(e) if e.is::<sharing::Cancelled>() => {
                    d.finish_transfer(&tid, TransferState::Cancelled, None)
                }
                Err(e) => d.finish_transfer(&tid, TransferState::Failed, Some(format!("{e:#}"))),
            }
        });
        Ok(id)
    }

    async fn connect_and_send(
        self: &Arc<Self>,
        tid: &str,
        dev: &Device,
        payload: Outgoing,
        cancel: CancellationToken,
    ) -> Result<TransferState> {
        let mut errors = Vec::new();
        let mut stream: Option<(BoxStream, &str)> = None;
        if let Some((addr, _)) = dev.lan {
            match lan::connect(addr).await {
                Ok(s) => stream = Some((Box::new(s), "wifi")),
                Err(e) => errors.push(format!("Wi-Fi: {e:#}")),
            }
        }
        if stream.is_none()
            && let (Some((mac, _)), Some(bt)) = (dev.bt, self.bt.get())
        {
            // The phone may ask to pair when we connect over Bluetooth Classic.
            self.allow_pairing(crate::bluetooth::format_mac(mac));
            match bt.connect(mac).await {
                Ok(s) => stream = Some((Box::new(s), "bluetooth")),
                Err(e) => errors.push(format!("Bluetooth: {e:#}")),
            }
        }
        let Some((stream, medium)) = stream else {
            if errors.is_empty() {
                bail!("{} cannot be reached", dev.name);
            }
            bail!("{}", errors.join("; "));
        };
        self.update_transfer(tid, |t| t.medium = medium.to_owned());
        sharing::run_outbound(Arc::clone(self), tid.to_owned(), stream, payload, cancel).await
    }

    // ----- temporary Bluetooth pairings (see dux.rs) -----

    /// Expect a pairing request from the phone we start sending to.
    fn allow_pairing(&self, address: String) {
        let until = Instant::now() + Duration::from_secs(180);
        self.lock()
            .pairing_allowed
            .insert(address.to_uppercase(), until);
    }

    /// Whether a "Just Works" pairing request from `address` comes from the
    /// phone the user is sending to. Every other request is left to the user
    /// (Dux's Bluetooth agent).
    pub async fn should_accept_pairing(&self, address: &str) -> bool {
        let (allowed, rate_ok) = {
            let mut st = self.lock();
            let now = Instant::now();
            st.recent_pairing_accepts
                .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
            st.pairing_allowed.retain(|_, until| *until > now);
            let allowed = st.config.use_bluetooth
                && st
                    .pairing_allowed
                    .contains_key(&address.trim().to_uppercase());
            (allowed, st.recent_pairing_accepts.len() < 5)
        };
        if !(allowed && rate_ok) {
            return false;
        }
        let Some(bt) = self.bt.get() else {
            return false;
        };
        let Some(path) = bt.device_path(address) else {
            return false;
        };
        // Never touch devices the user paired on purpose.
        !bt.is_paired(&path).await
    }

    pub fn note_temporary_pairing(&self, address: &str) {
        let Some(path) = self.bt.get().and_then(|bt| bt.device_path(address)) else {
            return;
        };
        let mut st = self.lock();
        st.recent_pairing_accepts.push(Instant::now());
        st.pairing_allowed.remove(&address.trim().to_uppercase());
        st.temp_pairings.insert(path, Instant::now());
    }

    /// Drop a temporary pairing once no Quick Share session uses it anymore.
    async fn release_temporary_pairing(&self, device_path: &str, force: bool) {
        let remove = {
            let mut st = self.lock();
            let busy = st.bt_sessions.get(device_path).copied().unwrap_or(0) > 0;
            debug!(
                "Temporary pairing {device_path}: known={} busy={busy} force={force}",
                st.temp_pairings.contains_key(device_path)
            );
            if st.temp_pairings.contains_key(device_path) && (force || !busy) {
                st.temp_pairings.remove(device_path);
                true
            } else {
                false
            }
        };
        if remove && let Some(bt) = self.bt.get() {
            bt.remove_device(device_path).await;
        }
    }

    // ----- snapshot -----

    pub fn snapshot(&self) -> Value {
        let st = self.lock();
        let now = Instant::now();
        let devices: Vec<Value> = st
            .devices
            .values()
            .filter(|d| !d.name.is_empty())
            .map(|d| {
                let mut mediums = Vec::new();
                if d.lan.is_some_and(|(_, t)| now.duration_since(t) < LAN_DEVICE_TTL) {
                    mediums.push("wifi");
                }
                if d.bt.is_some() {
                    mediums.push("bluetooth");
                }
                json!({ "id": d.id, "name": d.name, "type": d.device_type, "mediums": mediums, "rssi": d.rssi })
            })
            .collect();
        let bt: BtStatus = self.bt.get().map(|b| b.status()).unwrap_or_default();
        json!({
            "version": VERSION,
            "config": st.config,
            "visible": st.visible(),
            "visibleUntil": st.visible_until.filter(|_| st.config.visibility == Visibility::Temporary),
            "endpointId": st.local.endpoint_id,
            "downloadDir": drop_guard_dir(&st.config),
            "bluetooth": bt,
            "wifi": {
                "available": self.lan.get().is_some(),
                "upgrade": self.wifi.get().is_some() && st.config.wifi_upgrade,
                "port": self.lan.get().map(|l| l.port()),
                "interfaces": lan::connected_interfaces(),
            },
            "discovering": st.discovery_until.is_some(),
            "devices": devices,
            "transfers": st.transfers,
            "uiClients": st.ui_clients,
        })
    }
}

fn drop_guard_dir(cfg: &Config) -> String {
    let dir = cfg.download_dir.trim();
    if dir.is_empty() {
        util::default_download_dir()
    } else {
        util::expand_tilde(dir)
    }
    .to_string_lossy()
    .into_owned()
}

// ----------------------------------------------------------------------------
// Startup and background tasks
// ----------------------------------------------------------------------------

pub async fn run() -> Result<()> {
    let socket = util::socket_path();
    if UnixStream::connect(&socket).await.is_ok() {
        bail!(
            "another dms-quickshare daemon is already running ({})",
            socket.display()
        );
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("cannot create {}", socket.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }

    let config = load_config();
    if !config_file().exists() {
        save_config(&config);
    }
    let local = LocalEndpoint::new(&config.device_name);
    info!(
        "dms-quickshare {VERSION} starting as {:?} (endpoint {})",
        config.device_name, local.endpoint_id
    );
    let visible_until = (config.visibility == Visibility::Temporary)
        .then(|| util::now_ms() + u64::from(config.temporary_minutes) * 60_000);

    let (events, _) = broadcast::channel(256);
    let daemon = Arc::new(Daemon {
        state: Mutex::new(State {
            config: config.clone(),
            local,
            devices: BTreeMap::new(),
            transfers: Vec::new(),
            consent: HashMap::new(),
            cancel: HashMap::new(),
            visible_until,
            discovery_until: None,
            ui_clients: 0,
            next_id: 0,
            pairing_allowed: HashMap::new(),
            temp_pairings: HashMap::new(),
            bt_sessions: HashMap::new(),
            recent_pairing_accepts: Vec::new(),
        }),
        changed: Notify::new(),
        events,
        bt: OnceLock::new(),
        lan: OnceLock::new(),
        wifi: OnceLock::new(),
        notifier: OnceLock::new(),
        shutdown: CancellationToken::new(),
    });

    // Wi-Fi LAN
    let (lan_tx, lan_rx) = mpsc::channel(64);
    match Lan::start(config.port, lan_tx).await {
        Ok(lan) => {
            let _ = daemon.lan.set(lan);
        }
        Err(e) => warn!("Wi-Fi sharing unavailable: {e:#}"),
    }
    tokio::spawn(lan_events(Arc::clone(&daemon), lan_rx));
    if let Some(wifi) = Wifi::new().await {
        let _ = daemon.wifi.set(wifi);
    }

    // Bluetooth
    let (bt_tx, bt_rx) = mpsc::channel(256);
    match Bluetooth::start(bt_tx).await {
        Ok(bt) => {
            if config.auto_enable_bluetooth
                && config.use_bluetooth
                && config.visibility != Visibility::Hidden
            {
                bt.request_enable(false);
            }
            let mut status = bt.status_watch();
            let d = Arc::clone(&daemon);
            tokio::spawn(async move {
                while status.changed().await.is_ok() {
                    d.mark_changed();
                }
            });
            let _ = daemon.bt.set(bt);
        }
        Err(e) => warn!("Bluetooth sharing unavailable: {e:#}"),
    }
    tokio::spawn(bt_events(Arc::clone(&daemon), bt_rx));

    // Notifications
    match Notifier::start(Arc::downgrade(&daemon)).await {
        Ok(n) => {
            let _ = daemon.notifier.set(n);
        }
        Err(e) => warn!("Desktop notifications unavailable: {e:#}"),
    }

    daemon.apply_radios();
    tokio::spawn(crate::dux::run_pairing_helper(Arc::clone(&daemon)));
    tokio::spawn(janitor(Arc::clone(&daemon)));
    tokio::spawn(broadcaster(Arc::clone(&daemon)));
    tokio::spawn(control_server(Arc::clone(&daemon), listener));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
    info!("Shutting down");
    daemon.shutdown.cancel();
    let leftovers: Vec<String> = daemon.lock().temp_pairings.keys().cloned().collect();
    for path in leftovers {
        daemon.release_temporary_pairing(&path, true).await;
    }
    if let Some(lan) = daemon.lan.get() {
        lan.shutdown();
    }
    if let Some(bt) = daemon.bt.get() {
        bt.shutdown().await;
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn lan_events(d: Arc<Daemon>, mut rx: mpsc::Receiver<LanEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            LanEvent::Incoming(stream, addr) => {
                debug!("Wi-Fi: connection from {addr}");
                tokio::spawn(sharing::run_inbound(
                    Arc::clone(&d),
                    Box::new(stream),
                    "wifi",
                ));
            }
            LanEvent::Found {
                endpoint_id,
                info,
                addr,
            } => {
                d.device_seen(endpoint_id, info, |dev| {
                    dev.lan = Some((addr, Instant::now()))
                });
            }
            LanEvent::Lost { endpoint_id } => {
                let mut st = d.lock();
                if let Some(dev) = st.devices.get_mut(&endpoint_id) {
                    dev.lan = None;
                }
                drop(st);
                d.mark_changed();
            }
        }
    }
}

async fn bt_events(d: Arc<Daemon>, mut rx: mpsc::Receiver<BtEvent>) {
    let mut last_nearby = Instant::now() - Duration::from_secs(3600);
    while let Some(ev) = rx.recv().await {
        match ev {
            BtEvent::Incoming(fd, device) => match BtStream::new(fd) {
                Ok(stream) => {
                    *d.lock().bt_sessions.entry(device.clone()).or_default() += 1;
                    let d = Arc::clone(&d);
                    tokio::spawn(async move {
                        sharing::run_inbound(Arc::clone(&d), Box::new(stream), "bluetooth").await;
                        {
                            let mut st = d.lock();
                            if let Some(n) = st.bt_sessions.get_mut(&device) {
                                *n = n.saturating_sub(1);
                            }
                        }
                        // Give the phone a moment to finish, then forget the pairing.
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        d.release_temporary_pairing(&device, false).await;
                    });
                }
                Err(e) => warn!("Bluetooth: bad incoming socket: {e}"),
            },
            BtEvent::L2cap(fd) => {
                let d = Arc::clone(&d);
                tokio::spawn(async move {
                    match crate::ble_l2cap::accept(fd).await {
                        Ok(Some(stream)) => sharing::run_inbound(d, stream, "ble").await,
                        Ok(None) => {}
                        Err(e) => debug!("BLE L2CAP: {e:#}"),
                    }
                });
            }
            BtEvent::Sighting(s) => {
                let id = s.endpoint.endpoint_id.clone();
                let discovering = d.lock().discovery_until.is_some();
                if discovering {
                    d.device_seen(id, s.endpoint.info, |dev| {
                        dev.bt = Some((s.endpoint.bt_mac, Instant::now()));
                        if s.rssi.is_some() {
                            dev.rssi = s.rssi;
                        }
                    });
                }
            }
            BtEvent::NearbySharing => {
                if last_nearby.elapsed() > Duration::from_secs(60) {
                    last_nearby = Instant::now();
                    d.emit(json!({ "event": "nearbySharing" }));
                }
            }
        }
    }
}

/// Expire stale devices, end discovery and temporary visibility on time.
async fn janitor(d: Arc<Daemon>) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let mut changed = false;
        let mut radios = false;
        {
            let mut st = d.lock();
            let now = Instant::now();
            for dev in st.devices.values_mut() {
                if dev
                    .bt
                    .is_some_and(|(_, t)| now.duration_since(t) > BLE_DEVICE_TTL)
                {
                    dev.bt = None;
                    changed = true;
                }
                if dev
                    .lan
                    .is_some_and(|(_, t)| now.duration_since(t) > LAN_DEVICE_TTL)
                {
                    dev.lan = None;
                    changed = true;
                }
            }
            let before = st.devices.len();
            st.devices
                .retain(|_, dev| dev.bt.is_some() || dev.lan.is_some());
            changed |= before != st.devices.len();
            if st.discovery_until.is_some_and(|t| t <= now) {
                st.discovery_until = None;
                radios = true;
            }
            if st.config.visibility == Visibility::Temporary && !st.visible() {
                info!("Temporary visibility ended");
                st.config.visibility = Visibility::Hidden;
                st.visible_until = None;
                save_config(&st.config);
                radios = true;
            }
        }
        if radios {
            d.apply_radios();
        } else if changed {
            d.mark_changed();
        }
        let stale: Vec<String> = {
            let st = d.lock();
            st.temp_pairings
                .iter()
                .filter(|(path, t)| {
                    t.elapsed() > Duration::from_secs(180)
                        && st.bt_sessions.get(*path).copied().unwrap_or(0) == 0
                })
                .map(|(path, _)| path.clone())
                .collect()
        };
        for path in stale {
            d.release_temporary_pairing(&path, false).await;
        }
    }
}

/// Push state snapshots to subscribed clients (debounced).
async fn broadcaster(d: Arc<Daemon>) {
    loop {
        d.changed.notified().await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        let snapshot = d.snapshot();
        d.emit(json!({ "event": "state", "state": snapshot }));
    }
}

// ----------------------------------------------------------------------------
// Control socket (newline-delimited JSON)
// ----------------------------------------------------------------------------

async fn control_server(d: Arc<Daemon>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_client(Arc::clone(&d), stream));
            }
            Err(e) => {
                error!("Control socket: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn handle_client(d: Arc<Daemon>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let (out_tx, mut out_rx) = mpsc::channel::<Arc<str>>(128);
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write.write_all(line.as_bytes()).await.is_err()
                || write.write_all(b"\n").await.is_err()
            {
                break;
            }
        }
    });
    let mut events = d.events.subscribe();
    let mut subscribed = false;
    let mut is_ui = false;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let request: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = out_tx.send(Arc::from(json!({ "ok": false, "error": format!("bad JSON: {e}") }).to_string())).await;
                        continue;
                    }
                };
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let cmd = request.get("cmd").and_then(Value::as_str).unwrap_or_default().to_owned();
                if cmd == "subscribe" {
                    subscribed = true;
                    if request.get("ui").and_then(Value::as_bool).unwrap_or(false) && !is_ui {
                        is_ui = true;
                        d.lock().ui_clients += 1;
                        d.mark_changed();
                    }
                }
                let reply = match handle_request(&d, &cmd, &request).await {
                    Ok(result) => json!({ "id": id, "ok": true, "result": result }),
                    Err(e) => json!({ "id": id, "ok": false, "error": format!("{e:#}") }),
                };
                if out_tx.send(Arc::from(reply.to_string())).await.is_err() {
                    break;
                }
            }
            ev = events.recv(), if subscribed => match ev {
                Ok(line) => {
                    if out_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => d.mark_changed(),
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    if is_ui {
        let mut st = d.lock();
        st.ui_clients = st.ui_clients.saturating_sub(1);
        drop(st);
        d.mark_changed();
    }
    drop(out_tx);
    let _ = writer.await;
}

fn str_arg<'a>(req: &'a Value, key: &str) -> Result<&'a str> {
    req.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing \"{key}\""))
}

async fn handle_request(d: &Arc<Daemon>, cmd: &str, req: &Value) -> Result<Value> {
    match cmd {
        "getState" | "subscribe" => Ok(d.snapshot()),
        "ping" => Ok(json!("pong")),
        "setConfig" => {
            let patch = req
                .get("config")
                .cloned()
                .ok_or_else(|| anyhow!("missing \"config\""))?;
            Ok(serde_json::to_value(d.apply_config(patch, true)?)?)
        }
        "setVisibility" => {
            let v: Visibility =
                serde_json::from_value(req.get("visibility").cloned().unwrap_or_default())
                    .context("visibility must be everyone, temporary or hidden")?;
            d.set_visibility(v, true);
            Ok(d.snapshot())
        }
        "setName" => {
            let name = str_arg(req, "name")?;
            Ok(serde_json::to_value(
                d.apply_config(json!({ "deviceName": name }), true)?,
            )?)
        }
        "startDiscovery" => {
            d.start_discovery(req.get("seconds").and_then(Value::as_u64).unwrap_or(60));
            Ok(Value::Null)
        }
        "stopDiscovery" => {
            d.stop_discovery();
            Ok(Value::Null)
        }
        "send" => {
            let device = str_arg(req, "device")?;
            let files: Vec<PathBuf> = req
                .get("files")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("missing \"files\""))?
                .iter()
                .filter_map(Value::as_str)
                .map(|f| PathBuf::from(f.strip_prefix("file://").unwrap_or(f)))
                .collect();
            if files.is_empty() {
                bail!("no files given");
            }
            Ok(json!({ "transferId": d.send(device, Outgoing::Files(files))? }))
        }
        "sendText" => {
            let device = str_arg(req, "device")?;
            let text = str_arg(req, "text")?;
            if text.is_empty() {
                bail!("text is empty");
            }
            Ok(json!({ "transferId": d.send(device, Outgoing::Text(text.to_owned()))? }))
        }
        "accept" => d
            .decide(str_arg(req, "transfer")?, true)
            .map(|()| Value::Null),
        "reject" => d
            .decide(str_arg(req, "transfer")?, false)
            .map(|()| Value::Null),
        "cancel" => {
            let id = str_arg(req, "transfer")?;
            if d.decide(id, false).is_err() {
                d.cancel_transfer(id)?;
            }
            Ok(Value::Null)
        }
        "dismiss" => {
            let id = str_arg(req, "transfer")?.to_owned();
            d.lock()
                .transfers
                .retain(|t| t.id != id || !t.state.finished());
            d.mark_changed();
            Ok(Value::Null)
        }
        "clearFinished" => {
            d.lock().transfers.retain(|t| !t.state.finished());
            d.mark_changed();
            Ok(Value::Null)
        }
        "wifiSetup" => {
            let recommended = match d.wifi() {
                Some(wifi) => wifi.helper_would_help().await,
                None => false,
            };
            Ok(json!({
                "helperInstalled": crate::wifi::helper_installed(),
                "helperRecommended": recommended,
            }))
        }
        "enableBluetooth" => {
            let bt =
                d.bt.get()
                    .ok_or_else(|| anyhow!("Bluetooth support is not available"))?;
            bt.request_enable(true);
            Ok(Value::Null)
        }
        "open" => {
            let t = d
                .transfer(str_arg(req, "transfer")?)
                .ok_or_else(|| anyhow!("unknown transfer"))?;
            open_transfer(
                &t,
                req.get("folder").and_then(Value::as_bool).unwrap_or(false),
            );
            Ok(Value::Null)
        }
        "requestSend" => {
            let files: Vec<String> = req
                .get("files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if d.lock().ui_clients == 0 {
                bail!("the Dux Shell plugin is not running");
            }
            d.emit(json!({ "event": "sendRequested", "files": files }));
            Ok(Value::Null)
        }
        other => bail!("unknown command {other:?}"),
    }
}

pub fn open_transfer(t: &Transfer, folder: bool) {
    if let Some(url) = t.text.as_deref().filter(|s| util::looks_like_url(s)) {
        util::open_path(url);
        return;
    }
    let first = t.saved_paths.first().map(PathBuf::from);
    match first {
        Some(p) if folder || t.saved_paths.len() > 1 => {
            let dir = p.parent().map(Path::to_path_buf).unwrap_or(p);
            util::open_path(&dir.to_string_lossy());
        }
        Some(p) => util::open_path(&p.to_string_lossy()),
        None => {}
    }
}
