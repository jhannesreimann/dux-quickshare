# Testing

How to test the daemon with a real phone, and what has been verified.

## Unit tests

```bash
cd daemon
cargo test
```

Covers the UKEY2 handshake, BLE advertisement and header encoding (checked
against a real Android advertisement), the bloom filter hash, Wi-Fi card
capability parsing, and a full bandwidth upgrade between two of our own
links over the local network while data flows in both directions.

## Running the daemon by hand

Stop the plugin's daemon first (or disable the plugin), then:

```bash
cd daemon
cargo build
RUST_LOG=debug,zbus=warn,mdns_sd=warn ./target/debug/dms-quickshare daemon
```

In a second terminal: `dms-quickshare status`, `dms-quickshare doctor`, and
`dms-quickshare send --to "Phone name" FILE`.

The log lines to look for:

- `Connected to <phone> over ble|bluetooth|wifi`: how the connection started
- `Upgrade: ...`: the bandwidth upgrade decisions
- `Wi-Fi: hosting temporary hotspot ...` / `Wi-Fi: joined ...`: the Wi-Fi link
- `Upgrade: transfer continues over wifi-direct|hotspot`: the switch worked

## Phone side

With USB debugging enabled, the phone's own Quick Share log is very useful:

```bash
adb logcat -c
adb logcat -v time | grep -E "NearbyConnections|NearbySharing|NearbyMediums|Mosey|wpa_supplicant"
```

Useful markers: `waited local acceptance for 30000 ms` (request not answered
in time), `BandwidthUpgradeManager`, `retryBandwidthUpgradeWhenConnectedToWifi`,
`Disconnect Wi-Fi because STA and mosey are running on the same band`.

Verify a received file with `sha256sum` on both sides
(`adb shell sha256sum "/sdcard/Download/Quick Share/<file>"`).

## Test matrix

Verified 2026-11-19 to 2026-11-25 on a Lenovo Legion 5 (Arch Linux,
Hyprland, Dux Shell 2.1.2; internal Realtek RTL8852AE Wi-Fi + Bluetooth,
optional Cudy USB Wi-Fi dongle RTL8821CU) with an OPPO Find X9 Pro
(ColorOS 17). Speeds are from accepting until the file was complete.

| # | Setup | Direction | Path | Result |
|---|---|---|---|---|
| 1 | Same network, Bluetooth off | PC → phone | mDNS + TCP | 60 MB in 7 s, checksum ok |
| 2 | Same network | phone → PC | mDNS + TCP | 147 MB in about 10 s |
| 3 | Same network, phone briefly off Wi-Fi when connecting | phone → PC | BLE, then retry frame | Was stuck on BLE before the retry handling; fixed, not retested in this exact situation |
| 4 | Different networks, idle second adapter | phone → PC | BLE, phone hosts Wi-Fi Direct, PC joins | Works, phone stays on its Wi-Fi, no pairing prompt |
| 5 | Different networks, idle second adapter | PC → phone | RFCOMM, phone hosts Wi-Fi Direct, PC joins | 60 MB in 17 s including tapping Accept, checksum ok |
| 6 | Different networks, single card (dongle unplugged), helper installed | phone → PC | BLE, PC hotspot `qsap0` next to its connection on the same channel | 147 MB in 10 to 15 s, PC stays connected |
| 7 | Different networks, single card, helper installed | PC → phone | RFCOMM, phone offers Wi-Fi Direct, PC declines, phone asks PC to host | 60 MB in 12 s, checksum ok |
| 8 | Wi-Fi upgrade disabled | phone → PC | BLE L2CAP only | 3.5 MB in 165 s (about 21 KB/s) |
| 9 | Request not answered on the phone | PC → phone | | Phone cancels after 30 s, PC reports "did not accept" |

Not tested yet: a card with two client interfaces (`qssta0` path), the
`wifiInterrupt` mode, Ethernet-only PCs, other phone brands, a vertical bar.

### Plugin UI (2026-11-26, same setup)

| Area | Checked |
|---|---|
| Panel | Send to a nearby phone via "Choose files"; accept and decline incoming requests; verification code shown on both sides; rename; visibility picker and the "visible for N min" countdown; long file names elide instead of wrapping |
| Settings | Name, visibility, duration, download folder (Open and Change), notification and connection toggles |
| Control Center tile | Toggles visible / hidden |
| Drag and drop | File from Thunar onto the bar icon opens the panel with the file waiting |
| Hand-over | `dms-quickshare send FILE` (no `--to`) opens the panel |
| IPC | `toggle`, `visibility`, `status`; invalid input rejected |
| Notifications | Accept / Decline with code; file names shown verbatim (no Markdown) |
| Daemon lifecycle | The plugin starts the installed daemon and restarts it when it exits |
| Install | `install.sh` from a fresh copy inside `~/.config/dms/plugins/quickshare` (registry layout): builds, installs, no duplicate link |

## Single-card test setup

1. Install the helper: `./install.sh --wifi-helper`.
2. Leave only one Wi-Fi card active (unplug a USB adapter, or
   `nmcli device set <iface> managed no` for the test).
3. Take the phone off the PC's network but keep its Wi-Fi switched on (turn
   off auto-reconnect for that network on the phone, or it rejoins as soon
   as sharing starts).
4. Check the helper by hand:
   ```bash
   echo "DIRECT-ZZ-QuickShare Testpass12345678" | \
     pkexec /usr/lib/dms-quickshare/wifi-helper start-ap wlan0 5180
   iw dev qsap0 info      # type AP, same channel as wlan0
   pkexec /usr/lib/dms-quickshare/wifi-helper stop-ap
   ```
   Use your connection's frequency (`iw dev wlan0 link`).
