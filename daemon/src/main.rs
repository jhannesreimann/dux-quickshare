//! dms-quickshare: Quick Share (Nearby Share) for Linux.
//!
//! Send and receive files, links and text with Android devices over
//! Bluetooth and Wi-Fi. Runs as a small daemon controlled through a Unix
//! socket by the Dux Shell plugin or the command line.

mod ble_l2cap;
mod bluetooth;
mod cli;
mod crypto;
mod daemon;
mod dux;
mod endpoint;
mod lan;
mod link;
mod notify;
mod proto;
mod sharing;
mod transport;
mod util;
mod wifi;

use anyhow::{Result, bail};
use serde_json::json;

const HELP: &str = "\
dms-quickshare — Quick Share for Linux (send and receive with Android)

USAGE:
    dms-quickshare daemon                     Run the background service
    dms-quickshare status                     Show name, visibility, radios, transfers
    dms-quickshare devices [SECONDS]          List nearby devices
    dms-quickshare send [--to NAME] FILE...   Send files (opens the shell picker without --to)
    dms-quickshare send --to NAME --text TEXT Send a text or link
    dms-quickshare visibility [everyone|temporary|hidden]
    dms-quickshare name [NEW NAME]            Show or change the device name
    dms-quickshare accept|reject|cancel ID    Answer or stop a transfer
    dms-quickshare bluetooth-on               Switch Bluetooth on
    dms-quickshare doctor                     Check the system setup
    dms-quickshare --version
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let daemon_mode = matches!(args.first().map(String::as_str), Some("daemon" | "run"));
    let default_level = if daemon_mode {
        "info,zbus=warn,mdns_sd=warn"
    } else {
        "warn"
    };
    let mut logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level));
    // journald adds its own timestamps.
    if std::env::var_os("JOURNAL_STREAM").is_some() || !daemon_mode {
        logger.format_timestamp(None);
    } else {
        logger.format_timestamp_millis();
    }
    logger.init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build();
    let result = match runtime {
        Ok(rt) => rt.block_on(dispatch(args)),
        Err(e) => Err(e.into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn dispatch(args: Vec<String>) -> Result<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[args.len().min(1)..];
    match cmd {
        "daemon" | "run" => daemon::run().await,
        "status" => cli::status().await,
        "devices" => cli::devices(rest.first().and_then(|s| s.parse().ok()).unwrap_or(15)).await,
        "send" => {
            let mut to = None;
            let mut text = None;
            let mut files = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--to" | "-t" => to = it.next().cloned(),
                    "--text" => text = it.next().cloned(),
                    _ => files.push(a.clone()),
                }
            }
            cli::send(to, text, files).await
        }
        "visibility" => {
            match rest.first() {
                Some(v) => {
                    cli::simple("setVisibility", json!({ "visibility": v })).await?;
                    println!("Visibility: {v}");
                }
                None => {
                    let st = cli::simple("getState", json!({})).await?;
                    println!(
                        "{}",
                        st["config"]["visibility"].as_str().unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        "name" => {
            if rest.is_empty() {
                let st = cli::simple("getState", json!({})).await?;
                println!(
                    "{}",
                    st["config"]["deviceName"].as_str().unwrap_or_default()
                );
            } else {
                let cfg = cli::simple("setName", json!({ "name": rest.join(" ") })).await?;
                println!("Name: {}", cfg["deviceName"].as_str().unwrap_or_default());
            }
            Ok(())
        }
        "accept" | "reject" | "cancel" => {
            let Some(id) = rest.first() else {
                bail!("usage: dms-quickshare {cmd} TRANSFER_ID")
            };
            cli::simple(cmd, json!({ "transfer": id }))
                .await
                .map(|_| ())
        }
        "bluetooth-on" => cli::simple("enableBluetooth", json!({})).await.map(|_| ()),
        "doctor" => cli::doctor().await,
        "-V" | "--version" | "version" => {
            println!("dms-quickshare {}", daemon::VERSION);
            Ok(())
        }
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        other => bail!("unknown command {other:?}\n\n{HELP}"),
    }
}
