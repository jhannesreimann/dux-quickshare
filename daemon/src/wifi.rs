//! Temporary Wi-Fi links for fast transfers ("bandwidth upgrade"): a private
//! hotspot hosted on a spare Wi-Fi adapter, or joining the phone's hotspot
//! with it. Only adapters that are idle (managed by NetworkManager but not
//! connected) are ever used, so the user's normal Wi-Fi connection is never
//! touched. Connections are created in memory only ("volatile") and bound to
//! this process, so NetworkManager removes them even if we crash.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use log::{debug, info, warn};
use zbus::{Connection, ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";

const DEVICE_TYPE_WIFI: u32 = 2;
const DEVICE_STATE_DISCONNECTED: u32 = 30;
const CAP_AP: u32 = 0x40;
const CAP_5GHZ: u32 = 0x400;
const ACTIVE_ACTIVATED: u32 = 2;
const ACTIVE_DEACTIVATED: u32 = 4;
const DEVICE_STATE_ACTIVATED: u32 = 100;

/// Optional root helper (installed by the install script) that adds a
/// virtual hotspot interface next to the normal Wi-Fi connection.
const HELPER: &str = "/usr/lib/dms-quickshare/wifi-helper";
const VIRTUAL_IFACE: &str = "qsap0";
const VIRTUAL_STA_IFACE: &str = "qssta0";

pub struct Wifi {
    conn: Connection,
}

/// A hotspot we host or a network we joined. Torn down when dropped.
pub struct Lease {
    conn: Connection,
    /// NetworkManager activation, or None for the helper-run hotspot.
    active: Option<OwnedObjectPath>,
    pub iface: String,
    pub local_ip: Ipv4Addr,
    /// Remove this virtual interface afterwards ("ap" or "sta").
    remove_iface: Option<&'static str>,
    /// (connection, device) to reconnect afterwards (short interruption mode).
    restore: Option<(OwnedObjectPath, OwnedObjectPath)>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let conn = self.conn.clone();
        let active = self.active.clone();
        let iface = self.iface.clone();
        let remove_iface = self.remove_iface;
        let restore = self.restore.take();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let Some(active) = active else {
                    // The single-radio hotspot run by the helper.
                    if run_helper(&["stop-ap"]).await {
                        info!("Wi-Fi: stopped the temporary hotspot on {iface}");
                    }
                    return;
                };
                if let Some((prev, dev)) = restore {
                    // Activating the previous network replaces the temporary one.
                    let root = ObjectPath::try_from("/").expect("valid path");
                    match conn
                        .call_method(
                            Some(NM),
                            NM_PATH,
                            Some(NM),
                            "ActivateConnection",
                            &(prev.as_ref(), dev.as_ref(), root),
                        )
                        .await
                    {
                        Ok(_) => info!("Wi-Fi: reconnecting {iface} to the previous network"),
                        Err(e) => warn!("Wi-Fi: could not reconnect {iface}: {e}"),
                    }
                    return;
                }
                match conn
                    .call_method(
                        Some(NM),
                        NM_PATH,
                        Some(NM),
                        "DeactivateConnection",
                        &(active.as_ref(),),
                    )
                    .await
                {
                    Ok(_) => info!("Wi-Fi: released temporary connection on {iface}"),
                    Err(e) => debug!("Wi-Fi: deactivating {iface}: {e}"),
                }
                if let Some(kind) = remove_iface {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    run_helper(&["remove", kind]).await;
                }
            });
        }
    }
}

async fn run_helper(args: &[&str]) -> bool {
    match tokio::process::Command::new("pkexec")
        .arg("--disable-internal-agent")
        .arg(HELPER)
        .args(args)
        .output()
        .await
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            debug!(
                "Wi-Fi helper {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            debug!("Wi-Fi helper unavailable: {e}");
            false
        }
    }
}

/// What extra virtual interfaces the radio behind an interface allows next
/// to its existing connection (from `iw phy ... info`).
#[derive(Debug, Default, Clone, Copy)]
struct RadioCaps {
    /// A hotspot next to the connection (same channel).
    ap: bool,
    /// A second client connection.
    sta: bool,
    /// Channels usable at the same time.
    channels: u32,
}

fn parse_combinations(text: &str) -> RadioCaps {
    let mut caps = RadioCaps::default();
    let Some(start) = text.find("valid interface combinations") else {
        return caps;
    };
    // Join each "* ..." combination with its continuation lines.
    let mut combos: Vec<String> = Vec::new();
    for line in text[start..].lines().skip(1) {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("* ") {
            combos.push(rest.to_owned());
        } else if t.starts_with("total") || t.starts_with("#{") {
            if let Some(last) = combos.last_mut() {
                last.push(' ');
                last.push_str(t);
            }
        } else {
            break;
        }
    }
    for combo in combos {
        let mut groups: Vec<(Vec<String>, u32)> = Vec::new();
        let mut total = 1;
        let mut channels = 1;
        let mut rest = combo.as_str();
        while let Some(i) = rest.find("#{") {
            let after = &rest[i + 2..];
            let Some(close) = after.find('}') else { break };
            let types: Vec<String> = after[..close]
                .split(',')
                .map(|t| t.trim().to_owned())
                .collect();
            let limit = after[close..]
                .split("<=")
                .nth(1)
                .and_then(|n| {
                    n.trim()
                        .split(|c: char| !c.is_ascii_digit())
                        .next()?
                        .parse()
                        .ok()
                })
                .unwrap_or(1);
            groups.push((types, limit));
            rest = &after[close..];
        }
        if let Some(t) = combo.split("total <=").nth(1) {
            total = t
                .trim()
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
        }
        if let Some(c) = combo.split("#channels <=").nth(1) {
            channels = c
                .trim()
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
        }
        if total < 2 {
            continue;
        }
        let has = |g: &(Vec<String>, u32), t: &str| g.0.iter().any(|x| x == t);
        let managed: Vec<&(Vec<String>, u32)> =
            groups.iter().filter(|g| has(g, "managed")).collect();
        let ap: Vec<&(Vec<String>, u32)> = groups.iter().filter(|g| has(g, "AP")).collect();
        let ap_ok = managed
            .iter()
            .any(|m| ap.iter().any(|a| !std::ptr::eq(*m, *a) || a.1 >= 2));
        let sta_ok = managed.iter().any(|m| m.1 >= 2);
        caps.ap |= ap_ok;
        caps.sta |= sta_ok;
        if ap_ok || sta_ok {
            caps.channels = caps.channels.max(channels);
        }
    }
    caps
}

async fn radio_caps(iface: &str) -> RadioCaps {
    let Ok(phy) = std::fs::read_link(format!("/sys/class/net/{iface}/phy80211")) else {
        return RadioCaps::default();
    };
    let Some(phy) = phy.file_name().map(|p| p.to_string_lossy().into_owned()) else {
        return RadioCaps::default();
    };
    let Ok(out) = tokio::process::Command::new("iw")
        .args(["phy", &phy, "info"])
        .output()
        .await
    else {
        return RadioCaps::default();
    };
    parse_combinations(&String::from_utf8_lossy(&out.stdout))
}

pub struct Hotspot {
    pub ssid: String,
    pub password: String,
    pub frequency: Option<u32>,
}

struct SpareDevice {
    path: OwnedObjectPath,
    iface: String,
    caps: u32,
}

fn random_string(len: usize, alphabet: &[u8]) -> String {
    let mut bytes = vec![0u8; len];
    rand::fill(bytes.as_mut_slice());
    bytes
        .iter()
        .map(|b| alphabet[*b as usize % alphabet.len()] as char)
        .collect()
}

/// A 5 GHz channel the adapter may start a network on (not "no IR", not
/// radar/DFS, not disabled), from `iw phy`.
async fn usable_5ghz_channel(iface: &str) -> Option<u32> {
    let phy = std::fs::read_link(format!("/sys/class/net/{iface}/phy80211")).ok()?;
    let phy = phy.file_name()?.to_string_lossy().into_owned();
    let out = tokio::process::Command::new("iw")
        .args(["phy", &phy, "info"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut usable = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("* ") else {
            continue;
        };
        let Some((freq, _)) = rest.split_once(" MHz") else {
            continue;
        };
        let Ok(freq) = freq.trim().parse::<f32>() else {
            continue;
        };
        let freq = freq as u32;
        if !(5150..=5895).contains(&freq)
            || line.contains("no IR")
            || line.contains("disabled")
            || line.contains("radar")
        {
            continue;
        }
        if let Some((ch, _)) = channel_for(freq) {
            usable.push(ch);
        }
    }
    // Prefer the common non-DFS channels phones support everywhere.
    usable
        .iter()
        .copied()
        .find(|c| [36, 40, 44, 48].contains(c))
        .or_else(|| usable.first().copied())
}

/// Frequencies the radio behind `iface` may start a network on (not "no IR",
/// not radar/DFS, not disabled).
async fn usable_channels(iface: &str) -> Vec<u32> {
    let Ok(phy) = std::fs::read_link(format!("/sys/class/net/{iface}/phy80211")) else {
        return Vec::new();
    };
    let Some(phy) = phy.file_name().map(|p| p.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let Ok(out) = tokio::process::Command::new("iw")
        .args(["phy", &phy, "info"])
        .output()
        .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("* ")?;
            let (freq, _) = rest.split_once(" MHz")?;
            let freq = freq.trim().parse::<f32>().ok()? as u32;
            (!line.contains("no IR") && !line.contains("disabled") && !line.contains("radar"))
                .then_some(freq)
        })
        .collect()
}

/// Width and center frequency of the channel `iface` is connected on.
async fn channel_width(iface: &str) -> Option<(u32, u32)> {
    let out = tokio::process::Command::new("iw")
        .args(["dev", iface, "info"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("channel"))?;
    let num_after = |key: &str| -> Option<u32> {
        let rest = line.split(key).nth(1)?;
        rest.trim()
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    Some((num_after("width:")?, num_after("center1:")?))
}

fn channel_for(freq: u32) -> Option<(u32, &'static str)> {
    match freq {
        2412..=2472 => Some(((freq - 2407) / 5, "bg")),
        2484 => Some((14, "bg")),
        5160..=5885 => Some(((freq - 5000) / 5, "a")),
        _ => None,
    }
}

impl Wifi {
    /// Connect to NetworkManager; None if it is not running.
    pub async fn new() -> Option<Arc<Self>> {
        let conn = Connection::system().await.ok()?;
        let wifi = Self { conn };
        match wifi.devices().await {
            Ok(_) => Some(Arc::new(wifi)),
            Err(e) => {
                debug!("Wi-Fi upgrade unavailable: NetworkManager not reachable ({e})");
                None
            }
        }
    }

    async fn prop(&self, path: &str, iface: &str, name: &str) -> Result<OwnedVariant> {
        let reply = self
            .conn
            .call_method(Some(NM), path, Some(PROPS), "Get", &(iface, name))
            .await?;
        Ok(reply.body().deserialize::<OwnedVariant>()?)
    }

    async fn prop_u32(&self, path: &str, iface: &str, name: &str) -> Result<u32> {
        Ok(u32::try_from(&self.prop(path, iface, name).await?)?)
    }

    async fn prop_path(&self, path: &str, iface: &str, name: &str) -> Result<OwnedObjectPath> {
        Ok(OwnedObjectPath::try_from(
            self.prop(path, iface, name).await?,
        )?)
    }

    async fn devices(&self) -> Result<Vec<OwnedObjectPath>> {
        let reply = self
            .conn
            .call_method(Some(NM), NM_PATH, Some(NM), "GetDevices", &())
            .await?;
        Ok(reply.body().deserialize::<Vec<OwnedObjectPath>>()?)
    }

    /// A managed Wi-Fi adapter that is currently not connected to anything.
    async fn spare_device(&self, need_ap: bool) -> Option<SpareDevice> {
        for path in self.devices().await.ok()? {
            let p = path.as_str();
            if self.prop_u32(p, DEVICE, "DeviceType").await.ok() != Some(DEVICE_TYPE_WIFI) {
                continue;
            }
            if self.prop_u32(p, DEVICE, "State").await.ok() != Some(DEVICE_STATE_DISCONNECTED) {
                continue;
            }
            let caps = self
                .prop_u32(p, WIRELESS, "WirelessCapabilities")
                .await
                .unwrap_or(0);
            if need_ap && caps & CAP_AP == 0 {
                continue;
            }
            let Ok(iface) = self
                .prop(p, DEVICE, "Interface")
                .await
                .and_then(|v| Ok(String::try_from(v)?))
            else {
                continue;
            };
            // Our virtual interfaces each have one job.
            if (!need_ap && iface == VIRTUAL_IFACE) || (need_ap && iface == VIRTUAL_STA_IFACE) {
                continue;
            }
            return Some(SpareDevice { path, iface, caps });
        }
        None
    }

    /// Wi-Fi devices currently connected to a network: (path, interface).
    async fn connected_devices(&self) -> Vec<(OwnedObjectPath, String)> {
        let mut out = Vec::new();
        for path in self.devices().await.unwrap_or_default() {
            let p = path.as_str();
            if self.prop_u32(p, DEVICE, "DeviceType").await.ok() != Some(DEVICE_TYPE_WIFI)
                || self.prop_u32(p, DEVICE, "State").await.ok() != Some(DEVICE_STATE_ACTIVATED)
            {
                continue;
            }
            if let Ok(iface) = self
                .prop(p, DEVICE, "Interface")
                .await
                .and_then(|v| Ok(String::try_from(v)?))
                && iface != VIRTUAL_IFACE
                && iface != VIRTUAL_STA_IFACE
            {
                out.push((path, iface));
            }
        }
        out
    }

    /// A connected device whose radio can also host a hotspot, if the
    /// optional helper is installed.
    async fn virtual_parent(&self, sta: bool) -> Option<(OwnedObjectPath, String, RadioCaps)> {
        if !std::path::Path::new(HELPER).exists() {
            return None;
        }
        for (path, iface) in self.connected_devices().await {
            let caps = radio_caps(&iface).await;
            if (sta && caps.sta) || (!sta && caps.ap) {
                return Some((path, iface, caps));
            }
        }
        None
    }

    async fn virtual_ap_parent(&self) -> Option<(OwnedObjectPath, String)> {
        self.virtual_parent(false).await.map(|(p, i, _)| (p, i))
    }

    /// Create a virtual interface next to `parent` and wait for NetworkManager.
    async fn create_virtual(&self, parent: &str, kind: &'static str) -> Result<SpareDevice> {
        let name = if kind == "sta" {
            VIRTUAL_STA_IFACE
        } else {
            VIRTUAL_IFACE
        };
        if !run_helper(&["add", parent, kind]).await {
            bail!("could not create a temporary Wi-Fi interface");
        }
        for _ in 0..30 {
            if let Some(dev) = self
                .spare_device(kind == "ap")
                .await
                .filter(|d| d.iface == name)
            {
                let _ = self
                    .conn
                    .call_method(
                        Some(NM),
                        dev.path.as_str(),
                        Some(PROPS),
                        "Set",
                        &(DEVICE, "Autoconnect", Variant::from(false)),
                    )
                    .await;
                info!("Wi-Fi: created {name} next to {parent}");
                return Ok(dev);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        run_helper(&["remove", kind]).await;
        bail!("NetworkManager did not pick up the temporary Wi-Fi interface")
    }

    /// Can we host a hotspot without disturbing the current connection?
    pub async fn can_host(&self) -> bool {
        self.spare_device(true).await.is_some() || self.virtual_ap_parent().await.is_some()
    }

    /// Can we join another device's network (without disturbing anything,
    /// unless a short interruption is allowed)?
    pub async fn can_join(&self, allow_interrupt: bool) -> bool {
        self.spare_device(false).await.is_some()
            || self.virtual_parent(true).await.is_some()
            || (allow_interrupt && !self.connected_devices().await.is_empty())
    }

    /// Scan on `dev` until an access point named `ssid` shows up.
    async fn find_ap(
        &self,
        dev: &SpareDevice,
        ssid: &str,
        timeout: Duration,
    ) -> Option<OwnedObjectPath> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_scan = None::<tokio::time::Instant>;
        loop {
            if last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
                last_scan = Some(tokio::time::Instant::now());
                let opts: HashMap<&str, Variant<'_>> =
                    HashMap::from([("ssids", Variant::from(vec![ssid.as_bytes().to_vec()]))]);
                if let Err(e) = self
                    .conn
                    .call_method(
                        Some(NM),
                        dev.path.as_str(),
                        Some(WIRELESS),
                        "RequestScan",
                        &(opts,),
                    )
                    .await
                {
                    debug!("Wi-Fi: scan request on {}: {e}", dev.iface);
                }
            }
            if let Ok(v) = self.prop(dev.path.as_str(), WIRELESS, "AccessPoints").await
                && let Ok(aps) = Vec::<OwnedObjectPath>::try_from(v)
            {
                for ap in aps {
                    let name = self
                        .prop(
                            ap.as_str(),
                            "org.freedesktop.NetworkManager.AccessPoint",
                            "Ssid",
                        )
                        .await
                        .ok()
                        .and_then(|v| Vec::<u8>::try_from(v).ok());
                    if name.as_deref() == Some(ssid.as_bytes()) {
                        return Some(ap);
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    async fn activate(
        &self,
        dev: &SpareDevice,
        settings: HashMap<&str, HashMap<&str, Variant<'_>>>,
        timeout: Duration,
    ) -> Result<Lease> {
        self.activate_on(dev, settings, None, timeout).await
    }

    async fn activate_on(
        &self,
        dev: &SpareDevice,
        settings: HashMap<&str, HashMap<&str, Variant<'_>>>,
        ap: Option<&OwnedObjectPath>,
        timeout: Duration,
    ) -> Result<Lease> {
        let options: HashMap<&str, Variant<'_>> = HashMap::from([
            ("persist", Variant::from("volatile")),
            ("bind-activation", Variant::from("dbus-client")),
        ]);
        let root = match ap {
            Some(p) => p.as_ref(),
            None => ObjectPath::try_from("/").expect("valid path"),
        };
        let reply = self
            .conn
            .call_method(
                Some(NM),
                NM_PATH,
                Some(NM),
                "AddAndActivateConnection2",
                &(settings, dev.path.as_ref(), root, options),
            )
            .await
            .context("NetworkManager refused the connection")?;
        let (_conn, active, _): (
            OwnedObjectPath,
            OwnedObjectPath,
            HashMap<String, OwnedVariant>,
        ) = reply.body().deserialize()?;
        // From here on the lease owns the activation and cleans it up.
        let mut lease = Lease {
            conn: self.conn.clone(),
            active: Some(active.clone()),
            iface: dev.iface.clone(),
            local_ip: Ipv4Addr::UNSPECIFIED,
            remove_iface: None,
            restore: None,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.prop_u32(active.as_str(), ACTIVE, "State").await {
                Ok(ACTIVE_ACTIVATED) => break,
                Ok(ACTIVE_DEACTIVATED) | Err(_) => {
                    bail!("the Wi-Fi connection on {} failed", dev.iface)
                }
                Ok(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("the Wi-Fi connection on {} timed out", dev.iface);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        lease.local_ip = self
            .device_ip(dev.path.as_str())
            .await
            .ok_or_else(|| anyhow!("no IPv4 address on {}", dev.iface))?;
        Ok(lease)
    }

    async fn device_ip(&self, device: &str) -> Option<Ipv4Addr> {
        let cfg = self.prop_path(device, DEVICE, "Ip4Config").await.ok()?;
        if cfg.as_str() == "/" {
            return None;
        }
        let data = self
            .prop(
                cfg.as_str(),
                "org.freedesktop.NetworkManager.IP4Config",
                "AddressData",
            )
            .await
            .ok()?;
        let list = Vec::<HashMap<String, OwnedVariant>>::try_from(data).ok()?;
        list.iter().find_map(|a| {
            String::try_from(a.get("address")?.try_clone().ok()?)
                .ok()?
                .parse()
                .ok()
        })
    }

    /// Host a WPA2 hotspot on a spare adapter. The peer connects to
    /// `lease.local_ip`.
    pub async fn host(&self) -> Result<(Lease, Hotspot)> {
        if let Some(dev) = self.spare_device(true).await {
            return self.host_on(dev).await;
        }
        let (parent_path, parent) = self
            .virtual_ap_parent()
            .await
            .ok_or_else(|| anyhow!("no Wi-Fi adapter that can host a hotspot"))?;
        // A single radio can only use one channel: host on the one we are on.
        let freq = self
            .ap_frequency(parent_path.as_str())
            .await
            .ok_or_else(|| anyhow!("cannot tell which channel {parent} uses"))?;
        self.host_with_helper(&parent, freq).await
    }

    /// A hotspot next to the connection on a single radio. NetworkManager
    /// cannot run one there (it insists on a second client interface the
    /// radio does not allow), so the helper runs it directly. It uses the
    /// connection's channel, and its width when the card allows that.
    async fn host_with_helper(&self, parent: &str, freq: u32) -> Result<(Lease, Hotspot)> {
        let mut plans = Vec::new();
        if let Some((width, center)) = channel_width(parent)
            .await
            .filter(|(w, _)| [40, 80].contains(w))
        {
            // Every 20 MHz piece of the wide channel must allow transmitting.
            let pieces: Vec<u32> = (0..width / 20)
                .map(|i| center - width / 2 + 10 + 20 * i)
                .collect();
            let usable = usable_channels(parent).await;
            if pieces.iter().all(|f| usable.contains(f)) {
                plans.push((width, center));
            } else {
                debug!(
                    "Wi-Fi: {width} MHz hotspot not allowed here (regulatory domain), using 20 MHz"
                );
            }
        }
        plans.push((20, freq));
        let mut last_err = anyhow!("no channel plan");
        for (width, center) in plans {
            match self.start_helper_ap(parent, freq, width, center).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    debug!("Wi-Fi: {width} MHz hotspot next to {parent} failed: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn start_helper_ap(
        &self,
        parent: &str,
        freq: u32,
        width: u32,
        center: u32,
    ) -> Result<(Lease, Hotspot)> {
        use tokio::io::AsyncWriteExt;
        let ssid = format!(
            "DIRECT-{}-QuickShare",
            random_string(2, b"ABCDEFGHJKLMNPQRSTUVWXYZ0123456789")
        );
        let password = random_string(
            16,
            b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789",
        );
        let mut child = tokio::process::Command::new("pkexec")
            .arg("--disable-internal-agent")
            .arg(HELPER)
            .args([
                "start-ap",
                parent,
                &freq.to_string(),
                &width.to_string(),
                &center.to_string(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("cannot run the Wi-Fi helper")?;
        // The passphrase goes over stdin, never on a command line.
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(format!("{ssid} {password}\n").as_bytes())
                .await?;
        }
        let out = tokio::time::timeout(Duration::from_secs(25), child.wait_with_output())
            .await
            .map_err(|_| anyhow!("the Wi-Fi helper timed out"))??;
        if !out.status.success() {
            run_helper(&["stop-ap"]).await;
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let local_ip: Ipv4Addr = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .context("unexpected helper output")?;
        info!(
            "Wi-Fi: hosting temporary hotspot {ssid} next to {parent} ({local_ip}, {freq} MHz, {width} MHz wide)"
        );
        let lease = Lease {
            conn: self.conn.clone(),
            active: None,
            iface: VIRTUAL_IFACE.to_owned(),
            local_ip,
            remove_iface: None,
            restore: None,
        };
        Ok((
            lease,
            Hotspot {
                ssid,
                password,
                frequency: Some(freq),
            },
        ))
    }

    async fn host_on(&self, dev: SpareDevice) -> Result<(Lease, Hotspot)> {
        let ssid = format!(
            "DIRECT-{}-QuickShare",
            random_string(2, b"ABCDEFGHJKLMNPQRSTUVWXYZ0123456789")
        );
        let password = random_string(
            16,
            b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789",
        );
        // 5 GHz is much faster, but only on channels the regulatory domain
        // allows us to transmit on first ("no IR" channels make NM time out).
        let mut plans: Vec<(&str, Option<u32>)> = Vec::new();
        if dev.caps & CAP_5GHZ != 0
            && let Some(ch) = usable_5ghz_channel(&dev.iface).await
        {
            plans.push(("a", Some(ch)));
        }
        plans.push(("bg", None));
        let mut last_err = anyhow!("no band available");
        for (band, channel) in plans {
            let settings = HashMap::from([
                (
                    "connection",
                    HashMap::from([
                        ("id", Variant::from("Quick Share hotspot")),
                        ("type", Variant::from("802-11-wireless")),
                        ("autoconnect", Variant::from(false)),
                        ("interface-name", Variant::from(dev.iface.as_str())),
                    ]),
                ),
                ("802-11-wireless", {
                    let mut w = HashMap::from([
                        ("ssid", Variant::from(ssid.as_bytes().to_vec())),
                        ("mode", Variant::from("ap")),
                        ("band", Variant::from(band)),
                    ]);
                    if let Some(ch) = channel {
                        w.insert("channel", Variant::from(ch));
                    }
                    w
                }),
                (
                    "802-11-wireless-security",
                    HashMap::from([
                        ("key-mgmt", Variant::from("wpa-psk")),
                        ("psk", Variant::from(password.as_str())),
                        ("proto", Variant::from(vec!["rsn"])),
                        ("pairwise", Variant::from(vec!["ccmp"])),
                        ("group", Variant::from(vec!["ccmp"])),
                    ]),
                ),
                ("ipv4", HashMap::from([("method", Variant::from("shared"))])),
                ("ipv6", HashMap::from([("method", Variant::from("ignore"))])),
            ]);
            match self.activate(&dev, settings, Duration::from_secs(15)).await {
                Ok(lease) => {
                    // Let the access point settle before phones look for it.
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    let frequency = self.ap_frequency(dev.path.as_str()).await;
                    info!(
                        "Wi-Fi: hosting temporary hotspot {ssid} on {} ({}, {})",
                        dev.iface,
                        lease.local_ip,
                        frequency
                            .map(|f| format!("{f} MHz"))
                            .unwrap_or_else(|| "unknown frequency".into())
                    );
                    return Ok((
                        lease,
                        Hotspot {
                            ssid,
                            password,
                            frequency,
                        },
                    ));
                }
                Err(e) => {
                    warn!(
                        "Wi-Fi: hotspot on {} (band {band}, channel {channel:?}) failed: {e:#}",
                        dev.iface
                    );
                    last_err = e;
                    // Give NetworkManager a moment to release the device.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        Err(last_err)
    }

    async fn ap_frequency(&self, device: &str) -> Option<u32> {
        let ap = self
            .prop_path(device, WIRELESS, "ActiveAccessPoint")
            .await
            .ok()?;
        if ap.as_str() == "/" {
            return None;
        }
        self.prop_u32(
            ap.as_str(),
            "org.freedesktop.NetworkManager.AccessPoint",
            "Frequency",
        )
        .await
        .ok()
    }

    /// Join another device's hotspot with a spare adapter (never becomes
    /// the default route and does not change DNS).
    pub async fn join(
        &self,
        ssid: &str,
        password: &str,
        frequency: Option<u32>,
        allow_interrupt: bool,
    ) -> Result<Lease> {
        let mut restore = None;
        let mut virtual_sta = false;
        let spare = self.spare_device(false).await;
        let parent = if spare.is_none() {
            self.virtual_parent(true).await
        } else {
            None
        };
        let dev = match spare {
            Some(dev) => dev,
            // Seamless on radios that allow a second client connection.
            None if parent.is_some() => {
                let (_, iface, caps) = parent.expect("checked");
                debug!(
                    "Wi-Fi: joining next to {iface} ({} channel(s) at once)",
                    caps.channels
                );
                virtual_sta = true;
                self.create_virtual(&iface, "sta").await?
            }
            None if allow_interrupt => {
                // Short interruption: borrow the connected adapter and put the
                // previous network back afterwards.
                let (path, iface) = self
                    .connected_devices()
                    .await
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("no Wi-Fi adapter"))?;
                let active = self
                    .prop_path(path.as_str(), DEVICE, "ActiveConnection")
                    .await?;
                let prev = self
                    .prop_path(active.as_str(), ACTIVE, "Connection")
                    .await?;
                info!("Wi-Fi: briefly leaving the current network on {iface} for a fast transfer");
                restore = Some((prev, path.clone()));
                let caps = self
                    .prop_u32(path.as_str(), WIRELESS, "WirelessCapabilities")
                    .await
                    .unwrap_or(0);
                SpareDevice { path, iface, caps }
            }
            None => bail!("no idle Wi-Fi adapter to join the network with"),
        };
        // Many 5 GHz channels only allow passive scanning, so look for the
        // network first instead of probing for it.
        let ap = self.find_ap(&dev, ssid, Duration::from_secs(6)).await;
        debug!(
            "Wi-Fi: {ssid} {} (announced at {} MHz)",
            if ap.is_some() {
                "found"
            } else {
                "not seen in scans"
            },
            frequency.unwrap_or(0)
        );
        let mut last_err = anyhow!("could not join {ssid}");
        for key_mgmt in ["wpa-psk", "sae"] {
            let mut wireless = HashMap::from([
                ("ssid", Variant::from(ssid.as_bytes().to_vec())),
                ("mode", Variant::from("infrastructure")),
            ]);
            if ap.is_none() {
                // Not seen yet: probe for it on the announced channel.
                wireless.insert("hidden", Variant::from(true));
                if let Some((channel, band)) = frequency.and_then(channel_for) {
                    wireless.insert("band", Variant::from(band));
                    wireless.insert("channel", Variant::from(channel));
                }
            }
            let settings = HashMap::from([
                (
                    "connection",
                    HashMap::from([
                        ("id", Variant::from("Quick Share link")),
                        ("type", Variant::from("802-11-wireless")),
                        ("autoconnect", Variant::from(false)),
                        ("interface-name", Variant::from(dev.iface.as_str())),
                    ]),
                ),
                ("802-11-wireless", wireless),
                (
                    "802-11-wireless-security",
                    HashMap::from([
                        ("key-mgmt", Variant::from(key_mgmt)),
                        ("psk", Variant::from(password)),
                    ]),
                ),
                (
                    "ipv4",
                    HashMap::from([
                        ("method", Variant::from("auto")),
                        ("never-default", Variant::from(true)),
                        ("ignore-auto-dns", Variant::from(true)),
                    ]),
                ),
                ("ipv6", HashMap::from([("method", Variant::from("ignore"))])),
            ]);
            match self
                .activate_on(&dev, settings, ap.as_ref(), Duration::from_secs(12))
                .await
            {
                Ok(mut lease) => {
                    info!("Wi-Fi: joined {ssid} on {} ({})", dev.iface, lease.local_ip);
                    lease.restore = restore;
                    lease.remove_iface = virtual_sta.then_some("sta");
                    return Ok(lease);
                }
                Err(e) => {
                    debug!("Wi-Fi: joining {ssid} with {key_mgmt} failed: {e:#}");
                    last_err = e;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        if virtual_sta {
            run_helper(&["remove", "sta"]).await;
        }
        Err(last_err)
    }
}

pub fn helper_installed() -> bool {
    std::path::Path::new(HELPER).exists()
}

impl Wifi {
    /// Whether installing the root helper would make fast transfers between
    /// different networks possible here: no idle adapter, but a connected
    /// card that can run a second interface next to its connection.
    pub async fn helper_would_help(&self) -> bool {
        if helper_installed() || self.spare_device(false).await.is_some() {
            return false;
        }
        match self.connected_devices().await.first() {
            Some((_, iface)) => {
                let caps = radio_caps(iface).await;
                caps.ap || caps.sta
            }
            None => false,
        }
    }
}

/// Human-readable check of the fast-transfer options (for `doctor`):
/// (status, text) where status is "ok", "warn" or "info".
pub async fn report(allow_interrupt: bool) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let Some(wifi) = Wifi::new().await else {
        out.push((
            "warn",
            "NetworkManager not reachable: transfers between different networks stay on Bluetooth"
                .into(),
        ));
        return out;
    };
    let dnsmasq = which("dnsmasq");
    if !dnsmasq {
        out.push((
            "warn",
            "dnsmasq is not installed: hosting a temporary hotspot needs it".into(),
        ));
    }
    if let Some(dev) = wifi.spare_device(false).await {
        out.push((
            "ok",
            format!(
                "Idle Wi-Fi adapter {}: fast transfers without touching your connection",
                dev.iface
            ),
        ));
        return out;
    }
    let helper = helper_installed();
    let connected = wifi.connected_devices().await;
    let Some((_, iface)) = connected.first() else {
        out.push((
            "info",
            "No Wi-Fi connection: the adapter is free for fast transfers".into(),
        ));
        return out;
    };
    let caps = radio_caps(iface).await;
    // The helper runs its own wpa_supplicant for the hotspot; NetworkManager
    // itself may use iwd instead.
    let wpa_supplicant = which("wpa_supplicant");
    if caps.sta && helper {
        out.push((
            "ok",
            format!("{iface} can join the phone's Wi-Fi Direct next to your connection (seamless)"),
        ));
    } else if caps.ap && helper && dnsmasq && wpa_supplicant {
        out.push(("ok", format!("{iface} hosts a temporary hotspot next to your connection (phone switches network briefly)")));
    } else if caps.ap && helper && !wpa_supplicant {
        out.push((
            "warn",
            format!("{iface} could host a temporary hotspot, but wpa_supplicant is not installed (the Wi-Fi helper needs it)"),
        ));
    } else if (caps.ap || caps.sta) && !helper {
        out.push((
            "warn",
            format!("{iface} could keep your connection during fast transfers: install the Wi-Fi helper (install.sh --wifi-helper)"),
        ));
    } else if allow_interrupt {
        out.push((
            "info",
            format!("{iface} briefly leaves your network for fast transfers (setting enabled)"),
        ));
    } else {
        out.push((
            "info",
            format!("{iface} cannot run a second link: between different networks transfers use Bluetooth (slow); enable \"short Wi-Fi interruption\" in the settings for speed"),
        ));
    }
    if let Ok(o) = tokio::process::Command::new("iw")
        .args(["reg", "get"])
        .output()
        .await
    {
        let text = String::from_utf8_lossy(&o.stdout);
        if text.lines().take(3).any(|l| l.contains("country 00")) {
            out.push((
                "warn",
                "No Wi-Fi country set (regulatory domain 00): hotspots are limited to fewer, narrower channels. Install wireless-regdb and set your country, e.g. WIRELESS_REGDOM=\"DE\" in /etc/conf.d/wireless-regdom".into(),
            ));
        }
    }
    out
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
        || std::path::Path::new("/usr/sbin").join(cmd).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combinations() {
        let rtw89 = "\tvalid interface combinations:\n\t\t * #{ managed } <= 1, #{ AP, P2P-client, P2P-GO } <= 1,\n\t\t   total <= 2, #channels <= 1\n\tHT Capability overrides:\n";
        let c = parse_combinations(rtw89);
        assert!(c.ap && !c.sta);
        assert_eq!(c.channels, 1);
        let mt7921 = "\tvalid interface combinations:\n\t\t * #{ IBSS } <= 1, #{ managed, AP, mesh, P2P-client, P2P-GO } <= 2, #{ P2P-device } <= 1,\n\t\t   total <= 3, #channels <= 2, STA/AP BI must match\n";
        let c = parse_combinations(mt7921);
        assert!(c.ap && c.sta);
        assert_eq!(c.channels, 2);
        assert!(!parse_combinations("no combinations here").ap);
    }
}
