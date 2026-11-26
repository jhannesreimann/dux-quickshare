//! Command line client for the daemon's control socket.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::util;

pub struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    pub async fn connect() -> Result<Self> {
        let path = util::socket_path();
        let stream = UnixStream::connect(&path).await.map_err(|_| {
            anyhow!("the Quick Share daemon is not running.\nEnable the Quick Share plugin in Dux Shell or run `dms-quickshare daemon`.")
        })?;
        let (read, write) = stream.into_split();
        Ok(Self {
            lines: BufReader::new(read).lines(),
            write,
            next_id: 1,
        })
    }

    pub async fn call(&mut self, cmd: &str, mut params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        if !params.is_object() {
            params = json!({});
        }
        params["id"] = json!(id);
        params["cmd"] = json!(cmd);
        self.write
            .write_all(format!("{params}\n").as_bytes())
            .await?;
        loop {
            let msg = self.next_message().await?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if msg.get("ok").and_then(Value::as_bool) == Some(true) {
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                bail!(
                    "{}",
                    msg.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed")
                );
            }
        }
    }

    pub async fn next_message(&mut self) -> Result<Value> {
        let line = self
            .lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("the daemon closed the connection"))?;
        serde_json::from_str(&line).context("invalid message from daemon")
    }

    /// Wait for the next "state" event.
    pub async fn next_state(&mut self, timeout: Duration) -> Result<Option<Value>> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(left, self.next_message()).await {
                Ok(msg) => {
                    let msg = msg?;
                    if msg.get("event").and_then(Value::as_str) == Some("state") {
                        return Ok(msg.get("state").cloned());
                    }
                }
                Err(_) => return Ok(None),
            }
        }
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn print_devices(state: &Value) {
    let devices = state["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        println!("  (none yet)");
    }
    for d in devices {
        let mediums: Vec<&str> = d["mediums"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        println!(
            "  {:<24} {:<7} via {:<15} id {}",
            s(&d, "name"),
            s(&d, "type"),
            mediums.join("+"),
            s(&d, "id")
        );
    }
}

pub async fn status() -> Result<()> {
    let mut c = Client::connect().await?;
    let st = c.call("getState", json!({})).await?;
    let cfg = &st["config"];
    println!("Quick Share {}", s(&st, "version"));
    println!("  Name:        {}", s(cfg, "deviceName"));
    let vis = s(cfg, "visibility");
    let visible = st["visible"].as_bool().unwrap_or(false);
    println!(
        "  Visibility:  {vis}{}",
        if visible { " (visible now)" } else { "" }
    );
    println!("  Downloads:   {}", s(&st, "downloadDir"));
    let bt = &st["bluetooth"];
    let bt_line = if !bt["available"].as_bool().unwrap_or(false) {
        format!(
            "unavailable{}",
            bt["error"]
                .as_str()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        )
    } else if bt["blocked"].as_bool().unwrap_or(false) {
        "off (blocked) — run `dms-quickshare bluetooth-on`".to_owned()
    } else if !bt["powered"].as_bool().unwrap_or(false) {
        "off — run `dms-quickshare bluetooth-on`".to_owned()
    } else {
        format!(
            "on {}{}",
            s(bt, "address"),
            if bt["advertising"].as_bool().unwrap_or(false) {
                ", advertising"
            } else {
                ""
            }
        )
    };
    println!("  Bluetooth:   {bt_line}");
    let ifaces: Vec<&str> = st["wifi"]["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    println!(
        "  Wi-Fi/LAN:   {} (TCP port {})",
        if ifaces.is_empty() {
            "not connected".to_owned()
        } else {
            ifaces.join(", ")
        },
        st["wifi"]["port"]
    );
    let transfers = st["transfers"].as_array().cloned().unwrap_or_default();
    if !transfers.is_empty() {
        println!("Transfers:");
        for t in transfers {
            println!(
                "  {:<4} {:<8} {:<20} {:<20} {}/{} bytes{}",
                s(&t, "id"),
                s(&t, "direction"),
                s(&t, "peerName"),
                s(&t, "state"),
                t["bytesDone"],
                t["totalBytes"],
                t["error"]
                    .as_str()
                    .map(|e| format!("  ({e})"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

pub async fn devices(seconds: u64) -> Result<()> {
    let mut c = Client::connect().await?;
    c.call("subscribe", json!({})).await?;
    c.call("startDiscovery", json!({ "seconds": seconds + 5 }))
        .await?;
    println!("Looking for nearby devices for {seconds}s (open Quick Share on your phone)...");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last = String::new();
    while let Some(st) = c
        .next_state(deadline.saturating_duration_since(Instant::now()))
        .await?
    {
        let key = st["devices"].to_string();
        if key != last {
            last = key;
            println!("Nearby:");
            print_devices(&st);
        }
    }
    c.call("stopDiscovery", json!({})).await?;
    Ok(())
}

fn absolute(files: &[String]) -> Result<Vec<String>> {
    files
        .iter()
        .map(|f| {
            let p = PathBuf::from(f.strip_prefix("file://").unwrap_or(f));
            let p = std::fs::canonicalize(&p)
                .with_context(|| format!("cannot find {}", p.display()))?;
            Ok(p.to_string_lossy().into_owned())
        })
        .collect()
}

async fn wait_for_device(c: &mut Client, target: &str, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let st = c.call("getState", json!({})).await?;
    let find = |st: &Value| {
        st["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| s(d, "id") == target || s(d, "name").eq_ignore_ascii_case(target))
            .cloned()
    };
    if let Some(d) = find(&st) {
        return Ok(d);
    }
    while let Some(st) = c
        .next_state(deadline.saturating_duration_since(Instant::now()))
        .await?
    {
        if let Some(d) = find(&st) {
            return Ok(d);
        }
    }
    bail!("{target} was not found nearby. Make sure Quick Share is open on it and set to receive.")
}

async fn pick_device(c: &mut Client) -> Result<Value> {
    println!("Looking for nearby devices (open Quick Share on your phone)...");
    let mut st = c.call("getState", json!({})).await?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while let Some(next) = c
        .next_state(deadline.saturating_duration_since(Instant::now()))
        .await?
    {
        st = next;
    }
    let devices = st["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        bail!("no nearby devices found");
    }
    for (i, d) in devices.iter().enumerate() {
        println!("  [{}] {} ({})", i + 1, s(d, "name"), s(d, "type"));
    }
    print!("Send to which device? ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let n: usize = line.trim().parse().context("not a number")?;
    devices
        .get(n.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| anyhow!("no such device"))
}

pub async fn send(to: Option<String>, text: Option<String>, files: Vec<String>) -> Result<()> {
    let files = absolute(&files)?;
    if text.is_none() && files.is_empty() {
        bail!("nothing to send: give files or --text");
    }
    let mut c = Client::connect().await?;
    if to.is_none() && text.is_none() {
        // Let the shell show its device picker if it is running.
        if c.call("requestSend", json!({ "files": files }))
            .await
            .is_ok()
        {
            println!("Opened the Quick Share picker in Dux Shell.");
            return Ok(());
        }
    }
    c.call("subscribe", json!({})).await?;
    c.call("startDiscovery", json!({ "seconds": 60 })).await?;
    let device = match &to {
        Some(target) => {
            println!("Looking for {target}...");
            wait_for_device(&mut c, target, Duration::from_secs(30)).await?
        }
        None => pick_device(&mut c).await?,
    };
    let result = match &text {
        Some(t) => {
            c.call("sendText", json!({ "device": s(&device, "id"), "text": t }))
                .await?
        }
        None => {
            c.call(
                "send",
                json!({ "device": s(&device, "id"), "files": files }),
            )
            .await?
        }
    };
    let tid = s(&result, "transferId").to_owned();
    println!("Sending to {}...", s(&device, "name"));
    let mut shown_pin = false;
    loop {
        let msg = c.next_message().await?;
        let transfer = match s(&msg, "event") {
            "state" => msg["state"]["transfers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|t| s(t, "id") == tid)
                .cloned(),
            "transferFinished" => Some(msg["transfer"].clone()),
            _ => None,
        };
        let Some(t) = transfer else { continue };
        if let (false, Some(pin)) = (shown_pin, t["pin"].as_str()) {
            println!("Waiting for {} to accept (code {pin})", s(&device, "name"));
            shown_pin = true;
        }
        match s(&t, "state") {
            "transferring" => {
                let done = t["bytesDone"].as_u64().unwrap_or(0);
                let total = t["totalBytes"].as_u64().unwrap_or(0).max(1);
                print!(
                    "\r  {:>3}%  {} / {}   ",
                    done * 100 / total,
                    util::human_size(done),
                    util::human_size(total)
                );
                std::io::stdout().flush()?;
            }
            "completed" => {
                println!("\nDone.");
                break;
            }
            "rejected" => bail!("{} declined", s(&device, "name")),
            "cancelled" => bail!("cancelled"),
            "failed" => bail!("{}", t["error"].as_str().unwrap_or("transfer failed")),
            _ => {}
        }
    }
    c.call("stopDiscovery", json!({})).await.ok();
    Ok(())
}

pub async fn simple(cmd: &str, params: Value) -> Result<Value> {
    Client::connect().await?.call(cmd, params).await
}

pub async fn doctor() -> Result<()> {
    let ok = |b: bool| if b { "✓" } else { "✗" };
    println!("dms-quickshare doctor\n");

    let daemon = Client::connect().await.is_ok();
    println!(
        "{} Daemon running ({})",
        ok(daemon),
        util::socket_path().display()
    );

    // Bluetooth
    let adapters: Vec<String> = std::fs::read_dir("/sys/class/bluetooth")
        .map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("hci"))
                .collect()
        })
        .unwrap_or_default();
    println!(
        "{} Bluetooth adapter: {}",
        ok(!adapters.is_empty()),
        if adapters.is_empty() {
            "none".into()
        } else {
            adapters.join(", ")
        }
    );
    let (soft, hard) = crate::bluetooth::rfkill_state();
    println!(
        "{} Bluetooth radio {}",
        ok(!soft && !hard),
        if hard {
            "is switched off by a hardware switch"
        } else if soft {
            "is soft-blocked (run `dms-quickshare bluetooth-on`)"
        } else {
            "is not blocked"
        }
    );
    let system = zbus::Connection::system().await.ok();
    let bluez = match &system {
        Some(c) => c
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &("org.bluez",),
            )
            .await
            .ok()
            .and_then(|m| m.body().deserialize::<bool>().ok())
            .unwrap_or(false),
        None => false,
    };
    println!(
        "{} bluetoothd (BlueZ) running{}",
        ok(bluez),
        if bluez {
            ""
        } else {
            " — `sudo systemctl enable --now bluetooth`"
        }
    );

    // Network
    let ifaces = crate::lan::connected_interfaces();
    println!(
        "{} Network: {}",
        ok(!ifaces.is_empty()),
        if ifaces.is_empty() {
            "no Wi-Fi/Ethernet connection (Bluetooth still works)".into()
        } else {
            ifaces.join(", ")
        }
    );
    let firewall = ["firewalld", "ufw", "nftables", "iptables"]
        .iter()
        .find(|unit| {
            std::process::Command::new("systemctl")
                .args(["is-active", "--quiet", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
    match firewall {
        Some(fw) => println!(
            "! Firewall {fw} is active: allow UDP 5353 (mDNS) and the TCP port shown by `dms-quickshare status` for Wi-Fi transfers"
        ),
        None => println!("✓ No firewall service active"),
    }

    // Fast transfers between different networks
    let interrupt = if daemon {
        simple("getState", json!({}))
            .await
            .ok()
            .and_then(|st| st["config"]["wifiInterrupt"].as_bool())
            .unwrap_or(false)
    } else {
        false
    };
    for (level, text) in crate::wifi::report(interrupt).await {
        let mark = match level {
            "ok" => "✓",
            "warn" => "!",
            _ => "·",
        };
        println!("{mark} {text}");
    }

    // Notifications
    let session = zbus::Connection::session().await.ok();
    let notifications = match &session {
        Some(c) => c
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &("org.freedesktop.Notifications",),
            )
            .await
            .ok()
            .and_then(|m| m.body().deserialize::<bool>().ok())
            .unwrap_or(false),
        None => false,
    };
    println!("{} Notification server available", ok(notifications));

    if daemon {
        let st = simple("getState", json!({})).await?;
        let bt = &st["bluetooth"];
        println!(
            "{} Bluetooth advertising (visible to phones){}",
            ok(bt["advertising"].as_bool().unwrap_or(false)),
            bt["error"]
                .as_str()
                .map(|e| format!(" — {e}"))
                .unwrap_or_default()
        );
        let dir = s(&st, "downloadDir").to_owned();
        let writable = std::fs::create_dir_all(&dir).is_ok() && tempfile_ok(&dir);
        println!("{} Download folder writable: {dir}", ok(writable));
    }
    Ok(())
}

fn tempfile_ok(dir: &str) -> bool {
    let p = PathBuf::from(dir).join(".dms-quickshare-write-test");
    let ok = std::fs::write(&p, b"").is_ok();
    let _ = std::fs::remove_file(&p);
    ok
}
