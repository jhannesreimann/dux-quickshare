//! Bluetooth support through BlueZ (D-Bus) and raw kernel sockets:
//! - BLE advertising so Android devices can see us (0xFEF3, Nearby BLE v2)
//! - BLE scanning to find Android devices in receive mode
//! - an RFCOMM profile for incoming Quick Share connections
//! - SDP lookup + RFCOMM connect for outgoing connections

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use log::{debug, info, warn};
use serde::Serialize;
use tokio::sync::{Mutex, mpsc, watch};
use zbus::message::Type as MsgType;
use zbus::{
    Connection, MatchRule, MessageStream, ObjectPath, OwnedObjectPath, OwnedVariant, Variant,
    interface,
};

use crate::endpoint::{
    self, BLE_COPRESENCE_UUID, BLE_SHARING_UUID, BleEndpoint, FAST_INIT_DATA, GATT_SLOT0_UUID,
    RFCOMM_UUID,
};
use crate::transport::BtStream;

const BLUEZ: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const OM_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const PROFILE_PATH: &str = "/dev/dmsquickshare/bt/profile";
const ADV_PATH: &str = "/dev/dmsquickshare/bt/adv_receive";
const HEADER_ADV_PATH: &str = "/dev/dmsquickshare/bt/adv_header";
const FAST_INIT_PATH: &str = "/dev/dmsquickshare/bt/adv_share";
const GATT_ROOT: &str = "/dev/dmsquickshare/bt/gatt";
const GATT_SERVICE_PATH: &str = "/dev/dmsquickshare/bt/gatt/service0";
const GATT_CHAR_PATH: &str = "/dev/dmsquickshare/bt/gatt/service0/char0";

/// What the daemon wants the Bluetooth side to do right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desired {
    /// Advertise ourselves (Quick Share receive mode). `None` = hidden.
    pub visible: Option<endpoint::LocalEndpoint>,
    /// Scan for Android devices (send mode).
    pub scan: bool,
    /// Tell nearby Android devices that we want to share (send mode).
    pub fast_init: bool,
}

/// One-shot request to switch the radio on. Never re-applied automatically,
/// so turning Bluetooth off yourself is always respected.
#[derive(Clone, Copy, Debug)]
pub struct EnableRequest {
    /// Also lift an rfkill soft block (only for explicit user actions).
    pub unblock: bool,
}

impl PartialEq for endpoint::LocalEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint_id == other.endpoint_id && self.endpoint_info() == other.endpoint_info()
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BtStatus {
    /// A Bluetooth adapter and bluetoothd are present.
    pub available: bool,
    pub powered: bool,
    /// Bluetooth is switched off through rfkill (airplane mode or a toggle).
    pub blocked: bool,
    pub address: Option<String>,
    pub advertising: bool,
    pub scanning: bool,
    pub connectable: bool,
    pub error: Option<String>,
    #[serde(skip)]
    pub adapter_path: Option<String>,
}

/// A device seen in a BLE scan.
#[derive(Debug, Clone)]
pub struct BleSighting {
    pub endpoint: BleEndpoint,
    pub rssi: Option<i16>,
}

pub enum BtEvent {
    /// An accepted BLE L2CAP channel (no pairing involved).
    L2cap(OwnedFd),
    Sighting(BleSighting),
    /// A nearby Android device started sharing (fast initiation seen).
    NearbySharing,
    /// Incoming RFCOMM connection (socket, BlueZ device path).
    Incoming(OwnedFd, String),
}

pub struct Bluetooth {
    conn: Connection,
    desired: watch::Sender<Desired>,
    status: watch::Receiver<BtStatus>,
    enable: mpsc::Sender<EnableRequest>,
    connect_lock: Mutex<()>,
}

// ----------------------------------------------------------------------------
// D-Bus objects we export to BlueZ
// ----------------------------------------------------------------------------

struct NearbyProfile {
    events: mpsc::Sender<BtEvent>,
}

#[interface(name = "org.bluez.Profile1")]
impl NearbyProfile {
    fn release(&self) {}

    async fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::OwnedFd,
        _fd_properties: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<()> {
        info!(
            "Bluetooth: incoming RFCOMM connection from {}",
            device.as_str()
        );
        let fd: OwnedFd = fd.into();
        self.events
            .send(BtEvent::Incoming(fd, device.as_str().to_owned()))
            .await
            .map_err(|_| zbus::fdo::Error::Failed("daemon is shutting down".into()))
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

fn service_data_map(uuid: &str, data: &[u8]) -> HashMap<String, OwnedVariant> {
    let value = OwnedVariant::try_from(&Variant::from(data.to_vec()))
        .expect("byte arrays are always valid");
    HashMap::from([(uuid.to_owned(), value)])
}

/// Extended (BLE 5) advertisement: needed for payloads over 31 bytes, which
/// is what Android uses for its own Quick Share receive advertisement.
struct ExtendedAdvertisement {
    service_uuid: String,
    data: Vec<u8>,
}

/// BLE 5 extended advertisement with the full Nearby advertisement. Mirrors
/// Android's own receive set: connectable, not scannable, general
/// discoverable, service data only.
#[interface(name = "org.bluez.LEAdvertisement1")]
impl ExtendedAdvertisement {
    fn release(&self) {
        debug!("Bluetooth: advertisement released by BlueZ");
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".to_owned()
    }

    #[zbus(property)]
    fn service_data(&self) -> HashMap<String, OwnedVariant> {
        service_data_map(&self.service_uuid, &self.data)
    }

    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn secondary_channel(&self) -> String {
        "1M".to_owned()
    }

    #[zbus(property)]
    fn min_interval(&self) -> u32 {
        100
    }

    #[zbus(property)]
    fn max_interval(&self) -> u32 {
        100
    }
}

/// Legacy advertisement (fits in 31 bytes, seen by every phone).
struct LegacyAdvertisement {
    kind: &'static str,
    service_uuid: String,
    data: Vec<u8>,
    with_uuid_list: bool,
}

#[interface(name = "org.bluez.LEAdvertisement1")]
impl LegacyAdvertisement {
    fn release(&self) {}

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        self.kind.to_owned()
    }

    /// Empty when the payload needs every byte of the 31-byte legacy PDU.
    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        if self.with_uuid_list {
            vec![self.service_uuid.clone()]
        } else {
            Vec::new()
        }
    }

    #[zbus(property)]
    fn service_data(&self) -> HashMap<String, OwnedVariant> {
        service_data_map(&self.service_uuid, &self.data)
    }

    #[zbus(property)]
    fn min_interval(&self) -> u32 {
        100
    }

    #[zbus(property)]
    fn max_interval(&self) -> u32 {
        200
    }
}

/// Connectable, scannable, general-discoverable legacy advertisement with
/// the Base64 advertisement header, exactly like Android's own header set.
struct DiscoverableAdvertisement {
    service_uuid: String,
    data: Vec<u8>,
}

#[interface(name = "org.bluez.LEAdvertisement1")]
impl DiscoverableAdvertisement {
    fn release(&self) {}

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".to_owned()
    }

    #[zbus(property)]
    fn service_data(&self) -> HashMap<String, OwnedVariant> {
        service_data_map(&self.service_uuid, &self.data)
    }

    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn min_interval(&self) -> u32 {
        100
    }

    #[zbus(property)]
    fn max_interval(&self) -> u32 {
        100
    }
}

/// GATT service 0xFEF3 hosting the full advertisement for scanners that only
/// see the legacy header advertisement (what Nearby Connections does too).
struct GattService;

#[interface(name = "org.bluez.GattService1")]
impl GattService {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        BLE_COPRESENCE_UUID.to_owned()
    }

    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
}

struct GattSlotCharacteristic {
    value: Vec<u8>,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl GattSlotCharacteristic {
    fn read_value(&self, options: HashMap<String, OwnedVariant>) -> Vec<u8> {
        let offset = options
            .get("offset")
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0) as usize;
        let reader = options
            .get("device")
            .and_then(|v| OwnedObjectPath::try_from(v.clone()).ok())
            .and_then(|p| address_from_path(p.as_str()));
        debug!("Bluetooth: GATT read of the advertisement by {reader:?} (offset {offset})");
        self.value.get(offset..).unwrap_or_default().to_vec()
    }

    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        GATT_SLOT0_UUID.to_owned()
    }

    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(GATT_SERVICE_PATH).expect("valid path")
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["read".to_owned()]
    }
}

// ----------------------------------------------------------------------------
// Small D-Bus helpers
// ----------------------------------------------------------------------------

async fn get_prop(conn: &Connection, path: &str, iface: &str, name: &str) -> Result<OwnedVariant> {
    let reply = conn
        .call_method(Some(BLUEZ), path, Some(PROPS_IFACE), "Get", &(iface, name))
        .await?;
    Ok(reply.body().deserialize::<OwnedVariant>()?)
}

async fn set_prop(
    conn: &Connection,
    path: &str,
    iface: &str,
    name: &str,
    value: Variant<'_>,
) -> Result<()> {
    conn.call_method(
        Some(BLUEZ),
        path,
        Some(PROPS_IFACE),
        "Set",
        &(iface, name, value),
    )
    .await?;
    Ok(())
}

async fn get_bool(conn: &Connection, path: &str, iface: &str, name: &str) -> Result<bool> {
    Ok(bool::try_from(&get_prop(conn, path, iface, name).await?)?)
}

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;

async fn managed_objects(conn: &Connection) -> Result<ManagedObjects> {
    let reply = conn
        .call_method(Some(BLUEZ), "/", Some(OM_IFACE), "GetManagedObjects", &())
        .await?;
    Ok(reply.body().deserialize::<ManagedObjects>()?)
}

/// Path of the first Bluetooth adapter (hci0 normally).
async fn find_adapter(conn: &Connection) -> Result<Option<(String, Option<String>)>> {
    let objects = managed_objects(conn).await?;
    let mut adapters: Vec<(String, Option<String>)> = objects
        .iter()
        .filter_map(|(path, ifaces)| {
            let props = ifaces.get(ADAPTER_IFACE)?;
            let addr = props
                .get("Address")
                .and_then(|v| <&str>::try_from(v).ok())
                .map(str::to_owned);
            Some((path.as_str().to_owned(), addr))
        })
        .collect();
    adapters.sort();
    Ok(adapters.into_iter().next())
}

fn variant_bytes(v: &OwnedVariant) -> Option<Vec<u8>> {
    let mut value: &Variant<'_> = v;
    while let Variant::Variant(inner) = value {
        value = inner;
    }
    let owned = OwnedVariant::try_from(value).ok()?;
    Vec::<u8>::try_from(owned).ok()
}

fn service_data(v: &OwnedVariant) -> HashMap<String, Vec<u8>> {
    let Ok(cloned) = v.try_clone() else {
        return HashMap::new();
    };
    let Ok(map) = HashMap::<String, OwnedVariant>::try_from(cloned) else {
        return HashMap::new();
    };
    map.into_iter()
        .filter_map(|(k, v)| Some((k.to_lowercase(), variant_bytes(&v)?)))
        .collect()
}

// ----------------------------------------------------------------------------
// rfkill
// ----------------------------------------------------------------------------

/// (soft blocked, hard blocked) for Bluetooth radios.
pub fn rfkill_state() -> (bool, bool) {
    let mut soft = false;
    let mut hard = false;
    if let Ok(entries) = std::fs::read_dir("/sys/class/rfkill") {
        for e in entries.flatten() {
            let p = e.path();
            let read = |f: &str| {
                std::fs::read_to_string(p.join(f))
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            };
            if read("type") == "bluetooth" {
                soft |= read("soft") == "1";
                hard |= read("hard") == "1";
            }
        }
    }
    (soft, hard)
}

/// Lift the rfkill soft block of all Bluetooth radios (like `rfkill unblock bluetooth`).
pub fn rfkill_unblock() -> io::Result<()> {
    use std::io::Write;
    // struct rfkill_event { u32 idx; u8 type; u8 op; u8 soft; u8 hard; }
    const RFKILL_TYPE_BLUETOOTH: u8 = 2;
    const RFKILL_OP_CHANGE_ALL: u8 = 3;
    let mut event = [0u8; 8];
    event[4] = RFKILL_TYPE_BLUETOOTH;
    event[5] = RFKILL_OP_CHANGE_ALL;
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/rfkill")?
        .write_all(&event)
}

// ----------------------------------------------------------------------------
// Reconciliation: make BlueZ match `Desired`
// ----------------------------------------------------------------------------

#[derive(Default)]
struct Applied {
    adapter: Option<String>,
    profile: bool,
    adv: Option<Vec<u8>>,
    fast_init: bool,
    scanning: bool,
    /// Connectable value before we touched it (restored on shutdown).
    connectable_before: Option<bool>,
}

impl Bluetooth {
    pub async fn start(events: mpsc::Sender<BtEvent>) -> Result<Arc<Self>> {
        // Phones connect over BLE L2CAP without pairing; QS_BLE_L2CAP=0
        // falls back to Bluetooth Classic (RFCOMM) only.
        if !std::env::var("QS_BLE_L2CAP").is_ok_and(|v| v == "0") {
            match l2cap_le_listen() {
                Ok((fd, psm)) => {
                    info!("Bluetooth: accepting BLE L2CAP connections on PSM {psm:#x}");
                    let _ = L2CAP_PSM.set(psm);
                    l2cap_accept_loop(fd, events.clone());
                }
                Err(e) => warn!(
                    "Bluetooth: BLE L2CAP unavailable ({e}), phones will use Bluetooth Classic"
                ),
            }
        }
        let conn = Connection::system()
            .await
            .context("cannot connect to the system D-Bus")?;
        conn.object_server()
            .at(
                PROFILE_PATH,
                NearbyProfile {
                    events: events.clone(),
                },
            )
            .await
            .context("cannot export Bluetooth profile")?;
        let (desired, desired_rx) = watch::channel(Desired::default());
        let (status_tx, status) = watch::channel(BtStatus::default());
        let (enable, enable_rx) = mpsc::channel(4);
        let bt = Arc::new(Self {
            conn: conn.clone(),
            desired,
            status,
            enable,
            connect_lock: Mutex::new(()),
        });
        tokio::spawn(reconcile_loop(
            conn.clone(),
            desired_rx,
            enable_rx,
            status_tx,
        ));
        tokio::spawn(signal_loop(conn, events));
        Ok(bt)
    }

    /// BlueZ object path of a device on the current adapter.
    pub fn device_path(&self, address: &str) -> Option<String> {
        let adapter = self.status.borrow().adapter_path.clone()?;
        Some(format!(
            "{adapter}/dev_{}",
            address.trim().to_uppercase().replace(':', "_")
        ))
    }

    pub async fn is_paired(&self, device_path: &str) -> bool {
        get_bool(&self.conn, device_path, DEVICE_IFACE, "Paired")
            .await
            .unwrap_or(false)
    }

    /// Forget a device (drops a temporary Quick Share pairing).
    pub async fn remove_device(&self, device_path: &str) {
        let Some(adapter) = self.status.borrow().adapter_path.clone() else {
            debug!("Bluetooth: cannot remove {device_path}: no adapter");
            return;
        };
        let Ok(path) = ObjectPath::try_from(device_path) else {
            debug!("Bluetooth: cannot remove {device_path}: invalid path");
            return;
        };
        match self
            .conn
            .call_method(
                Some(BLUEZ),
                adapter.as_str(),
                Some(ADAPTER_IFACE),
                "RemoveDevice",
                &(path,),
            )
            .await
        {
            Ok(_) => info!("Bluetooth: removed temporary pairing {device_path}"),
            Err(e) => debug!("Bluetooth: RemoveDevice {device_path}: {e}"),
        }
    }

    /// Switch the adapter on (and optionally lift an rfkill soft block) once.
    pub fn request_enable(&self, unblock: bool) {
        let _ = self.enable.try_send(EnableRequest { unblock });
    }

    pub fn set_desired(&self, f: impl FnOnce(&mut Desired)) {
        self.desired.send_if_modified(|d| {
            let before = d.clone();
            f(d);
            *d != before
        });
    }

    pub fn status(&self) -> BtStatus {
        self.status.borrow().clone()
    }

    pub fn status_watch(&self) -> watch::Receiver<BtStatus> {
        self.status.clone()
    }

    /// Undo everything we changed in BlueZ (called on exit).
    pub async fn shutdown(&self) {
        self.set_desired(|d| *d = Desired::default());
        // Give the reconcile loop a moment to unregister everything.
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    /// Open an RFCOMM connection to a Quick Share receiver.
    pub async fn connect(&self, mac: [u8; 6]) -> Result<BtStream> {
        let _guard = self.connect_lock.lock().await;
        // Scanning slows down paging a lot on many controllers.
        let was_scanning = self.desired.borrow().scan;
        if was_scanning {
            self.set_desired(|d| d.scan = false);
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let result = async {
            let uuid = parse_uuid(RFCOMM_UUID);
            let mut last_err = anyhow!("no attempt made");
            for attempt in 0..3 {
                let channel =
                    tokio::task::spawn_blocking(move || sdp_find_rfcomm_channel(mac, uuid))
                        .await?
                        .with_context(|| {
                            format!(
                                "Bluetooth service lookup on {} failed",
                                endpoint::mac_to_string(&mac)
                            )
                        });
                match channel {
                    Ok(Some(ch)) => {
                        debug!("Bluetooth: Quick Share listens on RFCOMM channel {ch}");
                        match tokio::task::spawn_blocking(move || rfcomm_connect(mac, ch)).await? {
                            Ok(fd) => return Ok(BtStream::new(fd)?),
                            Err(e) => last_err = anyhow!(e).context("Bluetooth connection failed"),
                        }
                    }
                    Ok(None) => bail!("the device is not in Quick Share receive mode anymore"),
                    Err(e) => last_err = e,
                }
                debug!(
                    "Bluetooth connect attempt {} failed: {last_err:#}",
                    attempt + 1
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(last_err)
        }
        .await;
        if was_scanning {
            self.set_desired(|d| d.scan = true);
        }
        result
    }
}

async fn signal_loop(conn: Connection, events: mpsc::Sender<BtEvent>) {
    let rules = [
        MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BLUEZ)
            .interface(PROPS_IFACE)
            .member("PropertiesChanged")
            .path_namespace("/org/bluez")
            .build(),
        MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BLUEZ)
            .interface(OM_IFACE)
            .member("InterfacesAdded")
            .build(),
    ];
    let mut streams = Vec::new();
    for rule in rules {
        match rule {
            Ok(rule) => match MessageStream::for_match_rule(rule, &conn, None).await {
                Ok(s) => streams.push(s),
                Err(e) => warn!("Bluetooth: cannot subscribe to BlueZ signals: {e}"),
            },
            Err(e) => warn!("Bluetooth: bad match rule: {e}"),
        }
    }
    let mut merged = futures_util::stream::select_all(streams);
    while let Some(msg) = merged.next().await {
        let Ok(msg) = msg else { continue };
        let member = msg
            .header()
            .member()
            .map(|m| m.as_str().to_owned())
            .unwrap_or_default();
        let mut props: Option<HashMap<String, OwnedVariant>> = None;
        if member == "PropertiesChanged" {
            if let Ok((iface, changed, _)) =
                msg.body()
                    .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
                && iface == DEVICE_IFACE
            {
                props = Some(changed);
            }
        } else if member == "InterfacesAdded"
            && let Ok((_path, mut ifaces)) = msg.body().deserialize::<(
                OwnedObjectPath,
                HashMap<String, HashMap<String, OwnedVariant>>,
            )>()
        {
            props = ifaces.remove(DEVICE_IFACE);
        }
        let Some(props) = props else { continue };
        if let Some(connected) = props.get("Connected").and_then(|v| bool::try_from(v).ok()) {
            let path = msg
                .header()
                .path()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_default();
            debug!(
                "Bluetooth: {} {}",
                path.rsplit('/').next().unwrap_or_default(),
                if connected {
                    "connected"
                } else {
                    "disconnected"
                }
            );
        }
        let Some(sd) = props.get("ServiceData") else {
            continue;
        };
        let rssi = props.get("RSSI").and_then(|v| i16::try_from(v).ok());
        let data = service_data(sd);
        if let Some(adv) = data.get(BLE_COPRESENCE_UUID)
            && let Some(ep) = endpoint::parse_ble_advertisement(adv)
        {
            debug!(
                "BLE: {} ({}) in receive mode, rssi {rssi:?}",
                ep.info.name.as_deref().unwrap_or("?"),
                ep.endpoint_id
            );
            let _ = events.try_send(BtEvent::Sighting(BleSighting { endpoint: ep, rssi }));
        }
        if data.contains_key(BLE_SHARING_UUID) {
            let _ = events.try_send(BtEvent::NearbySharing);
        }
    }
    warn!("Bluetooth: signal stream ended");
}

/// Switch the radio on: lift a soft block if allowed, then power the adapter.
async fn enable_radio(conn: &Connection, req: EnableRequest) {
    let (soft, hard) = rfkill_state();
    if hard {
        warn!("Bluetooth: hardware switch is off, cannot enable");
        return;
    }
    if soft {
        if !req.unblock {
            debug!("Bluetooth: soft-blocked; waiting for an explicit user action");
            return;
        }
        info!("Bluetooth: lifting rfkill soft block");
        if let Err(e) = rfkill_unblock() {
            warn!("Bluetooth: rfkill unblock failed: {e}");
            return;
        }
        // bluetoothd re-creates the adapter after an unblock.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if matches!(find_adapter(conn).await, Ok(Some(_))) {
                break;
            }
        }
    }
    if let Ok(Some((adapter, _))) = find_adapter(conn).await
        && !get_bool(conn, &adapter, ADAPTER_IFACE, "Powered")
            .await
            .unwrap_or(false)
    {
        info!("Bluetooth: powering adapter on");
        if let Err(e) = set_prop(
            conn,
            &adapter,
            ADAPTER_IFACE,
            "Powered",
            Variant::from(true),
        )
        .await
        {
            warn!("Bluetooth: cannot power on: {e}");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn reconcile_loop(
    conn: Connection,
    mut desired_rx: watch::Receiver<Desired>,
    mut enable_rx: mpsc::Receiver<EnableRequest>,
    status_tx: watch::Sender<BtStatus>,
) {
    let mut applied = Applied::default();
    let mut owner_changes = match MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .arg(0, BLUEZ)
        .build()
    {
        Ok(rule) => MessageStream::for_match_rule(rule, &conn, None).await.ok(),
        Err(_) => None,
    };
    // Woken by what can change the outcome of apply(): the wanted state, the
    // adapter's properties, adapters coming and going, and bluetoothd
    // restarts. Only a failed registration is retried on a timer.
    let mut adapter_events = adapter_signals(&conn)
        .await
        .map(|s| s.filter(|m| std::future::ready(m.as_ref().is_ok_and(concerns_adapter))));
    let mut retry: Option<Duration> = None;
    loop {
        let desired = desired_rx.borrow_and_update().clone();
        let status = apply(&conn, &desired, &mut applied).await;
        retry = needs_retry(&desired, &applied, &status).then(|| {
            retry.map_or(Duration::from_secs(5), |d| {
                (d * 2).min(Duration::from_secs(60))
            })
        });
        status_tx.send_if_modified(|s| {
            if *s != status {
                *s = status;
                true
            } else {
                false
            }
        });
        let shutting_down = desired == Desired::default();
        if shutting_down
            && applied.adv.is_none()
            && !applied.profile
            && !applied.scanning
            && !applied.fast_init
        {
            restore_connectable(&conn, &mut applied).await;
        }
        tokio::select! {
            changed = desired_rx.changed() => if changed.is_err() { break },
            Some(req) = enable_rx.recv() => enable_radio(&conn, req).await,
            _ = async { match retry { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => {},
            Some(_) = async { match adapter_events.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {},
            Some(_) = async { match owner_changes.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                info!("Bluetooth: bluetoothd restarted, registering again");
                applied = Applied { connectable_before: applied.connectable_before, ..Default::default() };
            }
        }
    }
    restore_connectable(&conn, &mut applied).await;
}

/// BlueZ signals about adapters: property changes (Powered, Connectable, ...)
/// and adapters (or devices, filtered by `concerns_adapter`) appearing or
/// disappearing.
async fn adapter_signals(
    conn: &Connection,
) -> Option<futures_util::stream::SelectAll<MessageStream>> {
    let rules = [
        MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BLUEZ)
            .interface(PROPS_IFACE)
            .member("PropertiesChanged")
            .arg(0, ADAPTER_IFACE)
            .build(),
        MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BLUEZ)
            .interface(OM_IFACE)
            .member("InterfacesAdded")
            .build(),
        MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BLUEZ)
            .interface(OM_IFACE)
            .member("InterfacesRemoved")
            .build(),
    ];
    let mut streams = Vec::new();
    for rule in rules {
        match rule {
            Ok(rule) => match MessageStream::for_match_rule(rule, conn, None).await {
                Ok(s) => streams.push(s),
                Err(e) => warn!("Bluetooth: cannot subscribe to adapter signals: {e}"),
            },
            Err(e) => warn!("Bluetooth: bad match rule: {e}"),
        }
    }
    (!streams.is_empty()).then(|| futures_util::stream::select_all(streams))
}

/// Whether a signal can change the outcome of apply(): an adapter appearing
/// or disappearing (not a device; scans add and remove those all the time),
/// or a change of an adapter property apply() reads. Other properties, such
/// as the service list our own registrations change, are ignored.
fn concerns_adapter(msg: &zbus::Message) -> bool {
    const WATCHED: [&str; 3] = ["Powered", "Connectable", "Address"];
    let header = msg.header();
    match header.member().map(|m| m.as_str()) {
        Some("PropertiesChanged") => msg
            .body()
            .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
            .is_ok_and(|(_, changed, invalidated)| {
                WATCHED
                    .iter()
                    .any(|p| changed.contains_key(*p) || invalidated.iter().any(|i| i == p))
            }),
        Some("InterfacesAdded") => msg
            .body()
            .deserialize::<(
                OwnedObjectPath,
                HashMap<String, HashMap<String, OwnedVariant>>,
            )>()
            .is_ok_and(|(_, ifaces)| ifaces.contains_key(ADAPTER_IFACE)),
        Some("InterfacesRemoved") => msg
            .body()
            .deserialize::<(OwnedObjectPath, Vec<String>)>()
            .is_ok_and(|(_, ifaces)| ifaces.iter().any(|i| i == ADAPTER_IFACE)),
        _ => false,
    }
}

/// "AA:BB:CC:DD:EE:FF" from a BlueZ device path (".../dev_AA_BB_CC_DD_EE_FF").
fn address_from_path(path: &str) -> Option<String> {
    let dev = path.rsplit('/').next()?.strip_prefix("dev_")?;
    (dev.len() == 17).then(|| dev.replace('_', ":").to_uppercase())
}

/// "AA:BB:CC:DD:EE:FF" for a Bluetooth address.
pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Whether apply() left something undone on a usable adapter, which only
/// trying again can fix (a failed registration).
fn needs_retry(desired: &Desired, applied: &Applied, status: &BtStatus) -> bool {
    status.powered
        && ((desired.visible.is_some() && (!applied.profile || applied.adv.is_none()))
            || desired.fast_init != applied.fast_init
            || desired.scan != applied.scanning)
}

async fn restore_connectable(conn: &Connection, applied: &mut Applied) {
    if let (Some(before), Some(adapter)) =
        (applied.connectable_before.take(), applied.adapter.clone())
        && !before
    {
        let _ = set_prop(
            conn,
            &adapter,
            ADAPTER_IFACE,
            "Connectable",
            Variant::from(false),
        )
        .await;
    }
}

async fn apply(conn: &Connection, desired: &Desired, applied: &mut Applied) -> BtStatus {
    let (soft, hard) = rfkill_state();
    let mut status = BtStatus {
        blocked: soft || hard,
        ..Default::default()
    };

    let adapter = match find_adapter(conn).await {
        Ok(Some((path, addr))) => {
            status.address = addr;
            path
        }
        Ok(None) => {
            status.error = Some(
                if status.blocked {
                    "Bluetooth is switched off"
                } else {
                    "no Bluetooth adapter found"
                }
                .into(),
            );
            *applied = Applied {
                connectable_before: applied.connectable_before,
                ..Default::default()
            };
            return status;
        }
        Err(e) => {
            status.error = Some(format!("bluetoothd is not running ({e})"));
            *applied = Applied {
                connectable_before: applied.connectable_before,
                ..Default::default()
            };
            return status;
        }
    };
    status.available = true;
    status.adapter_path = Some(adapter.clone());
    if applied.adapter.as_deref() != Some(adapter.as_str()) {
        *applied = Applied {
            adapter: Some(adapter.clone()),
            ..Default::default()
        };
    }

    let powered = get_bool(conn, &adapter, ADAPTER_IFACE, "Powered")
        .await
        .unwrap_or(false);
    status.powered = powered && !status.blocked;
    if !status.powered {
        // Register again once powered (register_adv replaces what BlueZ kept).
        applied.adv = None;
        applied.fast_init = false;
        applied.scanning = false;
        return status;
    }

    // --- RFCOMM profile (incoming connections) ---
    let want_profile = desired.visible.is_some();
    if want_profile && !applied.profile {
        let mut opts: HashMap<&str, Variant<'_>> = HashMap::new();
        opts.insert("Name", Variant::from("NearbySharing"));
        opts.insert("Role", Variant::from("server"));
        opts.insert("RequireAuthentication", Variant::from(false));
        opts.insert("RequireAuthorization", Variant::from(false));
        opts.insert("AutoConnect", Variant::from(false));
        // BlueZ only listens on RFCOMM (and lists the channel in the SDP
        // record) when a channel is given; let the kernel pick a free one.
        match free_rfcomm_channel() {
            Ok(ch) => {
                debug!("Bluetooth: using RFCOMM channel {ch}");
                opts.insert("Channel", Variant::from(u16::from(ch)));
            }
            Err(e) => warn!("Bluetooth: cannot find a free RFCOMM channel: {e}"),
        }
        let path = ObjectPath::try_from(PROFILE_PATH).expect("valid path");
        match conn
            .call_method(
                Some(BLUEZ),
                "/org/bluez",
                Some("org.bluez.ProfileManager1"),
                "RegisterProfile",
                &(path, RFCOMM_UUID, opts),
            )
            .await
        {
            Ok(_) => {
                info!("Bluetooth: Quick Share RFCOMM service registered");
                applied.profile = true;
            }
            Err(e) if e.to_string().contains("AlreadyExists") => applied.profile = true,
            Err(e) => status.error = Some(format!("cannot register Bluetooth service: {e}")),
        }
    } else if !want_profile && applied.profile {
        let path = ObjectPath::try_from(PROFILE_PATH).expect("valid path");
        let _ = conn
            .call_method(
                Some(BLUEZ),
                "/org/bluez",
                Some("org.bluez.ProfileManager1"),
                "UnregisterProfile",
                &(path,),
            )
            .await;
        applied.profile = false;
    }

    // --- page scan so phones can connect to us ---
    let connectable = get_bool(conn, &adapter, ADAPTER_IFACE, "Connectable")
        .await
        .unwrap_or(true);
    status.connectable = connectable;
    if want_profile && !connectable {
        if applied.connectable_before.is_none() {
            applied.connectable_before = Some(connectable);
        }
        match set_prop(
            conn,
            &adapter,
            ADAPTER_IFACE,
            "Connectable",
            Variant::from(true),
        )
        .await
        {
            Ok(()) => status.connectable = true,
            Err(e) => warn!("Bluetooth: cannot make adapter connectable: {e}"),
        }
    }

    // --- receive advertisements (0xFEF3), mirroring Nearby Connections ---
    // 1. connectable extended advertisement carrying the full advertisement,
    // 2. GATT service exposing the same bytes in slot 0,
    // 3. connectable legacy advertisement with the header pointing at GATT.
    let psm = L2CAP_PSM.get().copied();
    let want_adv = desired.visible.as_ref().and_then(|ep| {
        let mac = status.address.as_deref().and_then(endpoint::parse_mac)?;
        // With a BLE L2CAP channel, leave out the classic address so phones
        // connect over LE (no pairing) instead of RFCOMM.
        let mac = if psm.is_some() { [0u8; 6] } else { mac };
        Some((ep.ble_advertisement(mac, psm), ep.clone()))
    });
    if want_adv.as_ref().map(|(adv, _)| adv) != applied.adv.as_ref() {
        if applied.adv.is_some() {
            unregister_adv(conn, &adapter, ADV_PATH).await;
            unregister_adv(conn, &adapter, HEADER_ADV_PATH).await;
            unregister_gatt(conn, &adapter).await;
            applied.adv = None;
        }
        if let Some((data, ep)) = want_adv {
            // Default: header + GATT only (what Quick Share for Windows does on
            // PCs without BLE 5). Android fetches it reliably; the extended
            // set can be added with QS_BLE_MODE=extended for experiments.
            let gatt_only = !std::env::var("QS_BLE_MODE").is_ok_and(|m| m == "extended");
            let extended = !gatt_only
                && match register_adv(
                    conn,
                    &adapter,
                    ADV_PATH,
                    AdvKind::Extended,
                    BLE_COPRESENCE_UUID,
                    data.clone(),
                )
                .await
                {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("Bluetooth: extended advertising failed ({e:#}), using GATT only");
                        false
                    }
                };
            let gatt = match register_gatt(conn, &adapter, data.clone()).await {
                Ok(()) => true,
                Err(e) => {
                    warn!("Bluetooth: GATT advertisement server failed: {e:#}");
                    false
                }
            };
            let header = gatt
                && match register_adv(
                    conn,
                    &adapter,
                    HEADER_ADV_PATH,
                    AdvKind::GattHeader,
                    BLE_COPRESENCE_UUID,
                    ep.ble_advertisement_header_b64(&data, extended, psm.unwrap_or(0)),
                )
                .await
                {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("Bluetooth: header advertisement failed: {e:#}");
                        false
                    }
                };
            if extended || header {
                info!(
                    "Bluetooth: advertising as visible ({} byte advertisement; extended: {extended}, GATT: {header})",
                    data.len()
                );
                applied.adv = Some(data);
            } else {
                status.error = Some("cannot advertise over Bluetooth LE".into());
            }
        }
    }
    status.advertising = applied.adv.is_some();

    // --- "I want to share" advertisement (0xFE2C) ---
    if desired.fast_init != applied.fast_init {
        if desired.fast_init {
            match register_adv(
                conn,
                &adapter,
                FAST_INIT_PATH,
                AdvKind::LegacyBroadcast,
                BLE_SHARING_UUID,
                FAST_INIT_DATA.to_vec(),
            )
            .await
            {
                Ok(()) => applied.fast_init = true,
                Err(e) => debug!("Bluetooth: fast initiation advertisement failed: {e:#}"),
            }
        } else {
            unregister_adv(conn, &adapter, FAST_INIT_PATH).await;
            applied.fast_init = false;
        }
    }

    // --- scanning ---
    if desired.scan != applied.scanning {
        if desired.scan {
            let mut filter: HashMap<&str, Variant<'_>> = HashMap::new();
            filter.insert("Transport", Variant::from("le"));
            filter.insert("DuplicateData", Variant::from(true));
            let _ = conn
                .call_method(
                    Some(BLUEZ),
                    adapter.as_str(),
                    Some(ADAPTER_IFACE),
                    "SetDiscoveryFilter",
                    &(filter,),
                )
                .await;
            match conn
                .call_method(
                    Some(BLUEZ),
                    adapter.as_str(),
                    Some(ADAPTER_IFACE),
                    "StartDiscovery",
                    &(),
                )
                .await
            {
                Ok(_) => applied.scanning = true,
                Err(e) if e.to_string().contains("InProgress") => applied.scanning = true,
                Err(e) => warn!("Bluetooth: cannot scan: {e}"),
            }
        } else {
            let _ = conn
                .call_method(
                    Some(BLUEZ),
                    adapter.as_str(),
                    Some(ADAPTER_IFACE),
                    "StopDiscovery",
                    &(),
                )
                .await;
            let empty: HashMap<&str, Variant<'_>> = HashMap::new();
            let _ = conn
                .call_method(
                    Some(BLUEZ),
                    adapter.as_str(),
                    Some(ADAPTER_IFACE),
                    "SetDiscoveryFilter",
                    &(empty,),
                )
                .await;
            applied.scanning = false;
        }
    }
    status.scanning = applied.scanning;
    status
}

#[derive(Clone, Copy)]
enum AdvKind {
    /// Non-connectable BLE 5 extended advertisement (up to 251 bytes).
    Extended,
    /// Connectable, discoverable legacy advertisement with the GATT header.
    GattHeader,
    /// Non-connectable legacy advertisement (31 bytes).
    LegacyBroadcast,
}

async fn register_adv(
    conn: &Connection,
    adapter: &str,
    path: &str,
    kind: AdvKind,
    uuid: &str,
    data: Vec<u8>,
) -> Result<()> {
    // BlueZ may still hold an earlier registration of this path (it keeps
    // advertisements over a power cycle); replace it.
    unregister_adv(conn, adapter, path).await;
    let service_uuid = uuid.to_owned();
    match kind {
        AdvKind::Extended => {
            conn.object_server()
                .at(path, ExtendedAdvertisement { service_uuid, data })
                .await?;
        }
        AdvKind::GattHeader => {
            conn.object_server()
                .at(path, DiscoverableAdvertisement { service_uuid, data })
                .await?;
        }
        AdvKind::LegacyBroadcast => {
            // UUID list (4) + service data (4 + n) must fit 31 bytes.
            let with_uuid_list = data.len() + 4 + 4 <= 31;
            conn.object_server()
                .at(
                    path,
                    LegacyAdvertisement {
                        kind: "broadcast",
                        service_uuid,
                        data,
                        with_uuid_list,
                    },
                )
                .await?;
        }
    }
    let opts: HashMap<&str, Variant<'_>> = HashMap::new();
    let obj = ObjectPath::try_from(path)?;
    if let Err(e) = conn
        .call_method(
            Some(BLUEZ),
            adapter,
            Some("org.bluez.LEAdvertisingManager1"),
            "RegisterAdvertisement",
            &(obj, opts),
        )
        .await
    {
        let _ = conn
            .object_server()
            .remove::<ExtendedAdvertisement, _>(path)
            .await;
        return Err(e.into());
    }
    Ok(())
}

async fn register_gatt(conn: &Connection, adapter: &str, value: Vec<u8>) -> Result<()> {
    unregister_gatt(conn, adapter).await;
    let server = conn.object_server();
    // Populate the tree first, then add the ObjectManager on top of it.
    server.at(GATT_SERVICE_PATH, GattService).await?;
    server
        .at(GATT_CHAR_PATH, GattSlotCharacteristic { value })
        .await?;
    server.at(GATT_ROOT, zbus::fdo::ObjectManager).await?;
    let opts: HashMap<&str, Variant<'_>> = HashMap::new();
    let root = ObjectPath::try_from(GATT_ROOT)?;
    conn.call_method(
        Some(BLUEZ),
        adapter,
        Some("org.bluez.GattManager1"),
        "RegisterApplication",
        &(root, opts),
    )
    .await?;
    Ok(())
}

async fn unregister_gatt(conn: &Connection, adapter: &str) {
    if let Ok(root) = ObjectPath::try_from(GATT_ROOT) {
        let _ = conn
            .call_method(
                Some(BLUEZ),
                adapter,
                Some("org.bluez.GattManager1"),
                "UnregisterApplication",
                &(root,),
            )
            .await;
    }
    let server = conn.object_server();
    let _ = server
        .remove::<zbus::fdo::ObjectManager, _>(GATT_ROOT)
        .await;
    let _ = server
        .remove::<GattSlotCharacteristic, _>(GATT_CHAR_PATH)
        .await;
    let _ = server.remove::<GattService, _>(GATT_SERVICE_PATH).await;
}

async fn unregister_adv(conn: &Connection, adapter: &str, path: &str) {
    if let Ok(obj) = ObjectPath::try_from(path) {
        let _ = conn
            .call_method(
                Some(BLUEZ),
                adapter,
                Some("org.bluez.LEAdvertisingManager1"),
                "UnregisterAdvertisement",
                &(obj,),
            )
            .await;
    }
    // Removal goes by interface name, which both advertisement types share.
    let _ = conn
        .object_server()
        .remove::<ExtendedAdvertisement, _>(path)
        .await;
}

// ----------------------------------------------------------------------------
// Kernel Bluetooth sockets: SDP over L2CAP and RFCOMM
// ----------------------------------------------------------------------------

const AF_BLUETOOTH: libc::c_int = 31;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_SECURITY: libc::c_int = 4;
const BT_RCVMTU: libc::c_int = 13;
const BDADDR_LE_PUBLIC: u8 = 1;

/// PSM of our BLE L2CAP listener, if enabled.
pub static L2CAP_PSM: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// Listen for BLE L2CAP (connection-oriented channel) connections on a
/// dynamic PSM, without requiring pairing or encryption.
pub fn l2cap_le_listen() -> io::Result<(OwnedFd, u16)> {
    let fd = bt_socket(libc::SOCK_SEQPACKET, BTPROTO_L2CAP, Duration::from_secs(0))?;
    let addr = SockaddrL2 {
        l2_family: AF_BLUETOOTH as libc::sa_family_t,
        l2_psm: 0,
        l2_bdaddr: [0; 6],
        l2_cid: 0,
        l2_bdaddr_type: BDADDR_LE_PUBLIC,
    };
    // SAFETY: plain socket calls with correctly sized structs.
    unsafe {
        let sec: [u8; 2] = [1, 0]; // BT_SECURITY_LOW, any key size
        libc::setsockopt(
            fd.as_raw_fd(),
            SOL_BLUETOOTH,
            BT_SECURITY,
            sec.as_ptr().cast(),
            2,
        );
        // Let phones send large SDUs (Linux defaults to 672 bytes).
        let mtu: u16 = 65535;
        libc::setsockopt(
            fd.as_raw_fd(),
            SOL_BLUETOOTH,
            BT_RCVMTU,
            (&mtu as *const u16).cast(),
            2,
        );
        if libc::bind(
            fd.as_raw_fd(),
            (&addr as *const SockaddrL2).cast(),
            std::mem::size_of::<SockaddrL2>() as libc::socklen_t,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        if libc::listen(fd.as_raw_fd(), 4) < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out: SockaddrL2 = std::mem::zeroed();
        let mut len = std::mem::size_of::<SockaddrL2>() as libc::socklen_t;
        if libc::getsockname(
            fd.as_raw_fd(),
            (&mut out as *mut SockaddrL2).cast(),
            &mut len,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((fd, u16::from_le(out.l2_psm)))
    }
}

/// Accept BLE L2CAP connections and hand them to the daemon.
fn l2cap_accept_loop(listener: OwnedFd, events: mpsc::Sender<BtEvent>) {
    std::thread::spawn(move || {
        loop {
            // SAFETY: accept on a listening socket we own.
            let c = unsafe {
                libc::accept(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if c < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                warn!("BLE L2CAP: accept failed: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            // SAFETY: accept returned a new descriptor we now own.
            let c = unsafe { OwnedFd::from_raw_fd(c) };
            if events.blocking_send(BtEvent::L2cap(c)).is_err() {
                return;
            }
        }
    });
}
const BTPROTO_L2CAP: libc::c_int = 0;
const BTPROTO_RFCOMM: libc::c_int = 3;

#[repr(C)]
struct SockaddrL2 {
    l2_family: libc::sa_family_t,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

#[repr(C)]
struct SockaddrRc {
    rc_family: libc::sa_family_t,
    rc_bdaddr: [u8; 6],
    rc_channel: u8,
}

fn bdaddr(mac: [u8; 6]) -> [u8; 6] {
    let mut b = mac;
    b.reverse();
    b
}

fn bt_socket(kind: libc::c_int, proto: libc::c_int, timeout: Duration) -> io::Result<OwnedFd> {
    // SAFETY: creating a socket and setting plain-old-data socket options.
    unsafe {
        let fd = libc::socket(AF_BLUETOOTH, kind | libc::SOCK_CLOEXEC, proto);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: 0,
        };
        for opt in [libc::SO_SNDTIMEO, libc::SO_RCVTIMEO] {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&tv as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        Ok(fd)
    }
}

fn connect_raw<T>(fd: &OwnedFd, addr: &T) -> io::Result<()> {
    // SAFETY: `addr` is a correctly laid out sockaddr for the socket's family.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (addr as *const T).cast(),
            std::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Ask the kernel for a free RFCOMM server channel (listen() on channel 0
/// assigns the lowest unused one), then release it for BlueZ to bind.
fn free_rfcomm_channel() -> io::Result<u8> {
    let fd = bt_socket(libc::SOCK_STREAM, BTPROTO_RFCOMM, Duration::from_secs(1))?;
    let addr = SockaddrRc {
        rc_family: AF_BLUETOOTH as libc::sa_family_t,
        rc_bdaddr: [0; 6],
        rc_channel: 0,
    };
    // SAFETY: binding/listening on our own socket with a valid sockaddr_rc.
    unsafe {
        if libc::bind(
            fd.as_raw_fd(),
            (&addr as *const SockaddrRc).cast(),
            std::mem::size_of::<SockaddrRc>() as libc::socklen_t,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        if libc::listen(fd.as_raw_fd(), 1) < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut bound = SockaddrRc {
            rc_family: 0,
            rc_bdaddr: [0; 6],
            rc_channel: 0,
        };
        let mut len = std::mem::size_of::<SockaddrRc>() as libc::socklen_t;
        if libc::getsockname(
            fd.as_raw_fd(),
            (&mut bound as *mut SockaddrRc).cast(),
            &mut len,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        if bound.rc_channel == 0 {
            return Err(io::Error::other("no free RFCOMM channel"));
        }
        Ok(bound.rc_channel)
    }
}

fn rfcomm_connect(mac: [u8; 6], channel: u8) -> io::Result<OwnedFd> {
    let fd = bt_socket(libc::SOCK_STREAM, BTPROTO_RFCOMM, Duration::from_secs(15))?;
    let addr = SockaddrRc {
        rc_family: AF_BLUETOOTH as libc::sa_family_t,
        rc_bdaddr: bdaddr(mac),
        rc_channel: channel,
    };
    connect_raw(&fd, &addr)?;
    Ok(fd)
}

fn parse_uuid(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
enum DataElement {
    Nil,
    UInt(u64),
    Uuid(Vec<u8>),
    Bytes(Vec<u8>),
    Bool(bool),
    Seq(Vec<DataElement>),
}

fn parse_data_element(buf: &[u8], pos: &mut usize) -> Option<DataElement> {
    let d = *buf.get(*pos)?;
    *pos += 1;
    let (ty, size_idx) = (d >> 3, d & 0x07);
    if ty == 0 {
        return Some(DataElement::Nil);
    }
    let len = match size_idx {
        0..=4 => [1usize, 2, 4, 8, 16][size_idx as usize],
        5..=7 => {
            let n = [1usize, 2, 4][(size_idx - 5) as usize];
            let raw = buf.get(*pos..*pos + n)?;
            *pos += n;
            raw.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize)
        }
        _ => return None,
    };
    let data = buf.get(*pos..*pos + len)?;
    *pos += len;
    Some(match ty {
        1 | 2 => DataElement::UInt(
            data.iter()
                .take(8)
                .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
        ),
        3 => DataElement::Uuid(data.to_vec()),
        5 => DataElement::Bool(data.first().copied().unwrap_or(0) != 0),
        6 | 7 => {
            let mut items = Vec::new();
            let mut p = 0;
            while p < data.len() {
                items.push(parse_data_element(data, &mut p)?);
            }
            DataElement::Seq(items)
        }
        _ => DataElement::Bytes(data.to_vec()),
    })
}

/// Find the RFCOMM channel from SDP attribute lists (ProtocolDescriptorList).
fn rfcomm_channel_from_records(records: &DataElement) -> Option<u8> {
    let DataElement::Seq(records) = records else {
        return None;
    };
    for record in records {
        let DataElement::Seq(attrs) = record else {
            continue;
        };
        for pair in attrs.chunks(2) {
            if let [DataElement::UInt(0x0004), DataElement::Seq(protocols)] = pair {
                for proto in protocols {
                    if let DataElement::Seq(items) = proto
                        && let [DataElement::Uuid(u), DataElement::UInt(ch), ..] = items.as_slice()
                        && u.as_slice() == [0x00, 0x03]
                    {
                        return u8::try_from(*ch).ok();
                    }
                }
            }
        }
    }
    None
}

fn sdp_find_rfcomm_channel(mac: [u8; 6], uuid: [u8; 16]) -> io::Result<Option<u8>> {
    let fd = bt_socket(libc::SOCK_SEQPACKET, BTPROTO_L2CAP, Duration::from_secs(12))?;
    let addr = SockaddrL2 {
        l2_family: AF_BLUETOOTH as libc::sa_family_t,
        l2_psm: 1u16.to_le(),
        l2_bdaddr: bdaddr(mac),
        l2_cid: 0,
        l2_bdaddr_type: 0,
    };
    connect_raw(&fd, &addr)?;

    let mut pattern = vec![0x35, 17, 0x1C];
    pattern.extend_from_slice(&uuid);
    let attr_ids = [0x35, 0x05, 0x0A, 0x00, 0x00, 0xFF, 0xFF];
    let mut acc = Vec::new();
    let mut continuation = vec![0u8];
    let mut buf = vec![0u8; 65536];
    for tid in 1u16..32 {
        let mut params = pattern.clone();
        params.extend_from_slice(&0xFFFFu16.to_be_bytes());
        params.extend_from_slice(&attr_ids);
        params.extend_from_slice(&continuation);
        let mut pdu = vec![0x06];
        pdu.extend_from_slice(&tid.to_be_bytes());
        pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
        pdu.extend_from_slice(&params);
        // SAFETY: sending from / receiving into valid buffers.
        let sent = unsafe {
            libc::send(
                fd.as_raw_fd(),
                pdu.as_ptr().cast(),
                pdu.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let resp = &buf[..n as usize];
        if resp.len() < 7 || resp[0] != 0x07 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected SDP response",
            ));
        }
        let count = u16::from_be_bytes([resp[5], resp[6]]) as usize;
        let lists = resp
            .get(7..7 + count)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short SDP response"))?;
        acc.extend_from_slice(lists);
        continuation = resp[7 + count..].to_vec();
        if continuation.first().copied().unwrap_or(0) == 0 {
            break;
        }
    }
    let mut pos = 0;
    Ok(
        parse_data_element(&acc, &mut pos)
            .and_then(|records| rfcomm_channel_from_records(&records)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(
            address_from_path("/org/bluez/hci0/dev_a4_c1_38_0f_22_9b").as_deref(),
            Some("A4:C1:38:0F:22:9B")
        );
        assert_eq!(address_from_path("/org/bluez/hci0"), None);
        assert_eq!(
            format_mac([0xa4, 0xc1, 0x38, 0x0f, 0x22, 0x9b]),
            "A4:C1:38:0F:22:9B"
        );
    }

    #[test]
    fn finds_rfcomm_channel() {
        // Attribute list shaped like the record of an Android phone in receive mode.
        let mut rec = vec![0x35, 0x00];
        let body: Vec<u8> = [
            vec![0x09, 0x00, 0x01, 0x35, 0x11, 0x1C],
            parse_uuid(RFCOMM_UUID).to_vec(),
            vec![
                0x09, 0x00, 0x04, 0x35, 0x0C, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00,
                0x03, 0x08, 22,
            ],
        ]
        .concat();
        rec[1] = body.len() as u8;
        rec.extend_from_slice(&body);
        let mut all = vec![0x35, rec.len() as u8];
        all.extend_from_slice(&rec);
        let mut pos = 0;
        let parsed = parse_data_element(&all, &mut pos).unwrap();
        assert_eq!(rfcomm_channel_from_records(&parsed), Some(22));
    }
}
