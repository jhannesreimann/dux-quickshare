//! Wi-Fi/Ethernet LAN medium: mDNS (DNS-SD) advertisement and discovery plus
//! the TCP listener. mDNS is restricted to physical network interfaces so
//! Docker/libvirt/VPN bridges are never advertised to phones.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, info, warn};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::endpoint::{self, LocalEndpoint, MDNS_SERVICE_TYPE, RemoteInfo};

pub enum LanEvent {
    Incoming(TcpStream, SocketAddr),
    Found {
        endpoint_id: String,
        info: RemoteInfo,
        addr: SocketAddr,
    },
    Lost {
        endpoint_id: String,
    },
}

pub struct Lan {
    mdns: ServiceDaemon,
    port: u16,
    /// (fullname, endpoint info) of the registered service.
    registered: Mutex<Option<(String, String)>>,
    browsing: Mutex<bool>,
    enabled_ifaces: Mutex<HashSet<String>>,
    events: mpsc::Sender<LanEvent>,
}

/// Names of physical network interfaces (Wi-Fi, Ethernet, USB tethering).
pub fn physical_interfaces() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // qs* are our own temporary Quick Share links.
            if name != "lo" && !name.starts_with("qs") && e.path().join("device").exists() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// Physical interfaces that currently have an IPv4 address.
pub fn connected_interfaces() -> Vec<String> {
    let mut out: Vec<String> = local_addresses().into_iter().map(|a| a.iface).collect();
    out.sort();
    out.dedup();
    out
}

/// An IPv4 address of a physical interface.
#[derive(Debug, Clone)]
pub struct LocalAddr {
    pub iface: String,
    pub ip: Ipv4Addr,
    pub prefix: u32,
}

impl LocalAddr {
    pub fn contains(&self, other: Ipv4Addr) -> bool {
        let mask = if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix.min(32))
        };
        u32::from(self.ip) & mask == u32::from(other) & mask
    }
}

/// IPv4 addresses on physical interfaces (no Docker/VPN bridges).
pub fn local_addresses() -> Vec<LocalAddr> {
    let physical: HashSet<String> = physical_interfaces().into_iter().collect();
    if_addrs()
        .into_iter()
        .filter(|(name, ..)| physical.contains(name))
        .map(|(iface, ip, prefix)| LocalAddr { iface, ip, prefix })
        .collect()
}

fn if_addrs() -> Vec<(String, Ipv4Addr, u32)> {
    let mut out = Vec::new();
    // SAFETY: standard getifaddrs/freeifaddrs usage; pointers are only read
    // while the list is alive.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned();
                let prefix = if ifa.ifa_netmask.is_null() {
                    24
                } else {
                    let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                    u32::from_be(m.sin_addr.s_addr).count_ones()
                };
                if !ip.is_loopback() {
                    out.push((name, ip, prefix));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

impl Lan {
    pub async fn start(port: u16, events: mpsc::Sender<LanEvent>) -> Result<Arc<Self>> {
        let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(l) => l,
            Err(e) if port != 0 => {
                warn!("Wi-Fi: port {port} unavailable ({e}), using a random port");
                TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await?
            }
            Err(e) => return Err(e).context("cannot open a TCP port"),
        };
        let port = listener.local_addr()?.port();
        info!("Wi-Fi: listening on TCP port {port}");

        let mdns = ServiceDaemon::new().context("cannot start mDNS")?;
        let _ = mdns.disable_interface(IfKind::All);
        let lan = Arc::new(Self {
            mdns,
            port,
            registered: Mutex::new(None),
            browsing: Mutex::new(false),
            enabled_ifaces: Mutex::new(HashSet::new()),
            events: events.clone(),
        });
        lan.refresh_interfaces();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nodelay(true);
                        if events.send(LanEvent::Incoming(stream, addr)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Wi-Fi: accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });

        let weak = Arc::downgrade(&lan);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                match weak.upgrade() {
                    Some(lan) => lan.refresh_interfaces(),
                    None => break,
                }
            }
        });
        Ok(lan)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Enable mDNS on newly appeared physical interfaces (IPv4 only).
    fn refresh_interfaces(&self) {
        let mut enabled = self.enabled_ifaces.lock().unwrap();
        let mut changed = false;
        for name in physical_interfaces() {
            if enabled.insert(name.clone()) {
                debug!("mDNS: enabling interface {name}");
                let _ = self.mdns.enable_interface(IfKind::Name(name));
                changed = true;
            }
        }
        if changed {
            // Rules apply in order; keep IPv6 off after every enable rule.
            let _ = self.mdns.disable_interface(IfKind::IPv6);
        }
    }

    pub fn set_visible(&self, local: Option<&LocalEndpoint>) {
        let mut registered = self.registered.lock().unwrap();
        let wanted = local.map(|l| (l.mdns_instance_name(), l.endpoint_info_b64()));
        let current = registered.as_ref().map(|(full, info)| {
            (
                full.split('.').next().unwrap_or_default().to_owned(),
                info.clone(),
            )
        });
        if wanted == current {
            return;
        }
        if let Some((fullname, _)) = registered.take() {
            let _ = self.mdns.unregister(&fullname);
        }
        let Some(local) = local else { return };
        let host = format!("quickshare-{}.local.", local.endpoint_id.to_lowercase());
        let props = [("n", local.endpoint_info_b64())];
        match ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &local.mdns_instance_name(),
            &host,
            (),
            self.port,
            &props[..],
        ) {
            Ok(info) => {
                let info = info.enable_addr_auto();
                let fullname = info.get_fullname().to_owned();
                match self.mdns.register(info) {
                    Ok(()) => {
                        info!("Wi-Fi: advertising via mDNS as {}", local.name);
                        *registered = Some((fullname, local.endpoint_info_b64()));
                    }
                    Err(e) => warn!("Wi-Fi: mDNS registration failed: {e}"),
                }
            }
            Err(e) => warn!("Wi-Fi: invalid mDNS service: {e}"),
        }
    }

    pub fn set_browsing(&self, on: bool, own_endpoint: String) {
        let mut browsing = self.browsing.lock().unwrap();
        if *browsing == on {
            return;
        }
        *browsing = on;
        if !on {
            let _ = self.mdns.stop_browse(MDNS_SERVICE_TYPE);
            return;
        }
        let receiver = match self.mdns.browse(MDNS_SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                warn!("Wi-Fi: mDNS browse failed: {e}");
                *browsing = false;
                return;
            }
        };
        let events = self.events.clone();
        std::thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                let msg = match event {
                    ServiceEvent::ServiceResolved(svc) => {
                        let instance = svc.fullname.split('.').next().unwrap_or_default();
                        let Some(endpoint_id) = endpoint::parse_mdns_instance_name(instance) else {
                            continue;
                        };
                        if endpoint_id == own_endpoint {
                            continue;
                        }
                        let Some(info) = svc
                            .get_property_val_str("n")
                            .and_then(endpoint::decode_b64)
                            .and_then(|b| endpoint::parse_endpoint_info(&b))
                        else {
                            continue;
                        };
                        let Some(ip) = svc.get_addresses_v4().into_iter().next() else {
                            continue;
                        };
                        LanEvent::Found {
                            endpoint_id,
                            info,
                            addr: SocketAddr::V4(SocketAddrV4::new(ip, svc.port)),
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let instance = fullname.split('.').next().unwrap_or_default();
                        let Some(endpoint_id) = endpoint::parse_mdns_instance_name(instance) else {
                            continue;
                        };
                        LanEvent::Lost { endpoint_id }
                    }
                    ServiceEvent::SearchStopped(_) => break,
                    _ => continue,
                };
                if events.blocking_send(msg).is_err() {
                    break;
                }
            }
        });
    }

    pub fn shutdown(&self) {
        self.set_visible(None);
        if let Ok(rx) = self.mdns.shutdown() {
            let _ = rx.recv_timeout(Duration::from_secs(1));
        }
    }
}

pub async fn connect(addr: SocketAddr) -> Result<TcpStream> {
    let stream = tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(addr))
        .await
        .context("connection timed out")?
        .with_context(|| format!("cannot connect to {addr}"))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}
