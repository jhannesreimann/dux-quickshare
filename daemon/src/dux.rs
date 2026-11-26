//! Integration with the Dux Shell (DMS) backend socket.
//!
//! When an Android phone connects to us over Bluetooth Classic it asks for a
//! "Just Works" pairing (no PIN). Dux's Bluetooth agent only shows that
//! request while the Control Center's Bluetooth page is open, so otherwise it
//! silently times out and the transfer fails. We answer such requests
//! ourselves, but only when they almost certainly belong to a Quick Share
//! connection: we are visible, the request is Just Works, the device is not
//! already paired, and a phone fetched our Quick Share advertisement moments
//! ago. The temporary pairing is removed again after the transfer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, info};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::daemon::Daemon;
use crate::util;

/// The DMS backend socket of the running Dux Shell session, if any.
fn find_socket() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DMS_SOCKET")
        .map(PathBuf::from)
        .filter(|p| p.exists())
    {
        return Some(p);
    }
    // Fall back to $XDG_RUNTIME_DIR/dms-<pid>.sock of a live process.
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> =
        std::fs::read_dir(util::runtime_dir())
            .ok()?
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let pid = name.strip_prefix("dms-")?.strip_suffix(".sock")?.to_owned();
                if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                std::path::Path::new("/proc")
                    .join(&pid)
                    .exists()
                    .then_some(())?;
                Some((e.metadata().ok()?.modified().ok()?, e.path()))
            })
            .collect();
    candidates.sort();
    candidates.pop().map(|(_, p)| p)
}

pub async fn run_pairing_helper(d: Arc<Daemon>) {
    loop {
        if let Some(path) = find_socket() {
            match session(&d, &path).await {
                Ok(()) => debug!("Dux: backend connection closed"),
                Err(e) => debug!("Dux: backend unavailable ({e:#})"),
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn session(d: &Arc<Daemon>, path: &std::path::Path) -> Result<()> {
    let stream = UnixStream::connect(path).await.context("connect")?;
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let subscribe = json!({
        "method": "subscribe",
        "params": { "clientId": "dms-quickshare", "services": ["bluetooth.pairing"] }
    });
    write.write_all(format!("{subscribe}\n").as_bytes()).await?;
    debug!("Dux: listening for Bluetooth pairing requests");
    let mut next_id: u64 = 1;
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(result) = msg.get("result") else {
            continue;
        };
        if result.get("service").and_then(Value::as_str) != Some("bluetooth.pairing") {
            continue;
        }
        let data = result.get("data").cloned().unwrap_or_default();
        let kind = data
            .get("requestType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let token = data
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let address = data
            .get("deviceAddr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let name = data
            .get("deviceName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if kind != "authorize" || token.is_empty() || address.is_empty() {
            continue;
        }
        if !d.should_accept_pairing(&address).await {
            debug!("Dux: leaving pairing request from {name} ({address}) to the user");
            continue;
        }
        info!(
            "Accepting Bluetooth pairing from {name} ({address}) for an incoming Quick Share connection"
        );
        let submit = json!({
            "id": next_id,
            "method": "bluetooth.pairing.submit",
            "params": { "token": token, "secrets": { "decision": "yes" }, "accept": true }
        });
        next_id += 1;
        write.write_all(format!("{submit}\n").as_bytes()).await?;
        d.note_temporary_pairing(&address);
    }
    Ok(())
}
