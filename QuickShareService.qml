pragma Singleton
pragma ComponentBehavior: Bound

import QtQuick
import Quickshell
import Quickshell.Io
import qs.Common
import qs.Services
import qs.DCommon.Common as DCommon

// Connection to the dms-quickshare daemon, shared by all plugin surfaces.
// The daemon does the Quick Share protocol work; this object keeps its state
// snapshot up to date and forwards commands. The daemon is started here when
// it is not already running (for example from the command line).
Singleton {
    id: root

    // Set by the daemon surface while the plugin is enabled.
    property bool enabled: false

    readonly property bool connected: socket.linkUp
    property string binaryPath: ""
    property bool binaryMissing: false
    property string lastError: ""

    // --- daemon state (see the "state" event in daemon/src/daemon.rs) ---
    property var state: ({})
    readonly property var config: state.config ?? ({})
    readonly property string deviceName: config.deviceName ?? ""
    readonly property string visibility: config.visibility ?? "everyone"
    readonly property bool isVisible: state.visible ?? false
    readonly property var visibleUntil: state.visibleUntil ?? null
    readonly property var bluetooth: state.bluetooth ?? ({})
    readonly property bool bluetoothOn: bluetooth.available === true && bluetooth.powered === true
    readonly property var wifi: state.wifi ?? ({})
    readonly property bool discovering: state.discovering ?? false
    readonly property string downloadDir: state.downloadDir ?? ""
    readonly property string version: state.version ?? ""
    readonly property var devices: state.devices ?? []
    readonly property var transfers: state.transfers ?? []
    readonly property var requests: transfers.filter(t => t.state === "awaiting_consent")
    readonly property var activeTransfers: transfers.filter(t => ["connecting", "awaiting_acceptance", "transferring"].includes(t.state))
    readonly property var finishedTransfers: transfers.filter(t => ["completed", "rejected", "cancelled", "failed"].includes(t.state)).slice().reverse()

    // Files waiting for a target device (drag and drop, picker, CLI).
    property var pendingFiles: []

    // Clock for the "visible for N min" countdown. Only ticks while needed,
    // so it is refreshed whenever a new deadline arrives.
    property real now: Date.now()

    onVisibleUntilChanged: now = Date.now()

    Timer {
        interval: 30000
        repeat: true
        triggeredOnStart: true
        running: root.isVisible && root.visibility === "temporary"
        onTriggered: root.now = Date.now()
    }

    signal sendRequested(var files)

    // --- commands ---

    function setVisibility(mode) {
        call("setVisibility", {
            "visibility": mode
        });
    }

    function setName(name) {
        call("setName", {
            "name": name
        });
    }

    function setConfig(patch) {
        call("setConfig", {
            "config": patch
        });
    }

    function startDiscovery(seconds) {
        call("startDiscovery", {
            "seconds": seconds
        });
    }

    function stopDiscovery() {
        call("stopDiscovery", {});
    }

    function sendFiles(deviceId, files) {
        call("send", {
            "device": deviceId,
            "files": files
        });
    }

    function sendText(deviceId, text) {
        call("sendText", {
            "device": deviceId,
            "text": text
        });
    }

    function accept(id) {
        call("accept", {
            "transfer": id
        });
    }

    function reject(id) {
        call("reject", {
            "transfer": id
        });
    }

    function cancel(id) {
        call("cancel", {
            "transfer": id
        });
    }

    function dismiss(id) {
        call("dismiss", {
            "transfer": id
        });
    }

    function clearFinished() {
        call("clearFinished", {});
    }

    function enableBluetooth() {
        call("enableBluetooth", {});
    }

    function open(id, folder) {
        call("open", {
            "transfer": id,
            "folder": folder
        });
    }

    // Accepts plain paths and file:// URLs (drag and drop).
    function setPendingFiles(files) {
        pendingFiles = (files || []).map(f => {
            const s = String(f);
            if (!s.startsWith("file://"))
                return s;
            try {
                return decodeURIComponent(s.slice(7));
            } catch (e) {
                return s.slice(7);
            }
        }).filter(f => f.length > 0);
    }

    // --- formatting helpers shared by the UI ---

    function formatBytes(bytes) {
        const n = Number(bytes) || 0;
        if (n < 1024)
            return I18n.trFor("quickshare", "%1 B").arg(n);
        const units = ["KB", "MB", "GB", "TB"];
        let value = n / 1024;
        let unit = 0;
        while (value >= 1024 && unit < units.length - 1) {
            value /= 1024;
            unit++;
        }
        return (value < 10 ? value.toFixed(1) : Math.round(value)) + " " + units[unit];
    }

    function mediumLabel(medium) {
        switch (medium) {
        case "wifi":
            return I18n.trFor("quickshare", "Wi-Fi");
        case "hotspot":
            return I18n.trFor("quickshare", "Wi-Fi hotspot");
        case "wifi-direct":
            return I18n.trFor("quickshare", "Wi-Fi Direct");
        case "ble":
        case "bluetooth":
            return I18n.trFor("quickshare", "Bluetooth");
        }
        return "";
    }

    function deviceIcon(type) {
        switch (type) {
        case "tablet":
            return "tablet_android";
        case "laptop":
            return "laptop";
        }
        return "smartphone";
    }

    function itemIcon(transfer) {
        const items = transfer?.items ?? [];
        if (items.length === 1) {
            const item = items[0];
            if (item.kind === "url")
                return "link";
            if (item.kind === "text")
                return "notes";
            if (item.kind === "wifi")
                return "wifi";
            if (item.mime?.startsWith("image/"))
                return "image";
            if (item.mime?.startsWith("video/"))
                return "movie";
            if (item.mime?.startsWith("audio/"))
                return "music_note";
        }
        return "description";
    }

    function summary(transfer) {
        const items = transfer?.items ?? [];
        if (items.length === 0)
            return "";
        if (items.length === 1) {
            const item = items[0];
            if (item.kind === "url")
                return I18n.trFor("quickshare", "A link");
            if (item.kind === "text")
                return I18n.trFor("quickshare", "A text");
            if (item.kind === "wifi")
                return I18n.trFor("quickshare", "Wi-Fi network \"%1\"").arg(item.name);
            return item.name;
        }
        if (items.every(i => i.mime?.startsWith("image/")))
            return I18n.trFor("quickshare", "%1 photos").arg(items.length);
        if (items.every(i => i.mime?.startsWith("video/")))
            return I18n.trFor("quickshare", "%1 videos").arg(items.length);
        return I18n.trFor("quickshare", "%1 files").arg(items.length);
    }

    function progress(transfer) {
        const total = Number(transfer?.totalBytes) || 0;
        if (total <= 0)
            return 0;
        return Math.max(0, Math.min(1, (Number(transfer.bytesDone) || 0) / total));
    }

    function stateLabel(transfer) {
        switch (transfer?.state) {
        case "connecting":
            return I18n.trFor("quickshare", "Connecting");
        case "awaiting_consent":
            return I18n.trFor("quickshare", "Waiting for you");
        case "awaiting_acceptance":
            return I18n.trFor("quickshare", "Waiting for %1 to accept").arg(transfer.peerName);
        case "transferring":
            return transfer.direction === "incoming" ? I18n.trFor("quickshare", "Receiving") : I18n.trFor("quickshare", "Sending");
        case "completed":
            return transfer.direction === "incoming" ? I18n.trFor("quickshare", "Received") : I18n.trFor("quickshare", "Sent");
        case "rejected":
            return I18n.trFor("quickshare", "Declined");
        case "cancelled":
            return I18n.trFor("quickshare", "Cancelled");
        case "failed":
            return I18n.trFor("quickshare", "Failed");
        }
        return "";
    }

    function visibilityLabel() {
        if (!isVisible)
            return I18n.trFor("quickshare", "Hidden");
        if (visibility === "temporary" && visibleUntil) {
            const minutes = Math.max(1, Math.ceil((visibleUntil - now) / 60000));
            return I18n.trFor("quickshare", "Visible for %1 min").arg(minutes);
        }
        return I18n.trFor("quickshare", "Visible to everyone");
    }

    // --- protocol ---

    property int _nextId: 1
    property var _callbacks: ({})

    function call(cmd, args, callback) {
        if (!socket.linkUp) {
            if (callback)
                callback(false, I18n.trFor("quickshare", "Quick Share is not running"));
            return;
        }
        const id = _nextId++;
        if (callback)
            _callbacks[id] = callback;
        socket.send(Object.assign({
            "id": id,
            "cmd": cmd
        }, args || {}));
    }

    function _handle(line) {
        let msg;
        try {
            msg = JSON.parse(line);
        } catch (e) {
            return;
        }
        if (msg.event) {
            switch (msg.event) {
            case "state":
                state = msg.state || {};
                break;
            case "sendRequested":
                setPendingFiles(msg.files || []);
                sendRequested(pendingFiles);
                break;
            }
            return;
        }
        if (msg.id === 0 && msg.ok && msg.result) {
            state = msg.result;
            return;
        }
        const callback = _callbacks[msg.id];
        if (callback) {
            delete _callbacks[msg.id];
            callback(msg.ok === true, msg.ok ? msg.result : msg.error);
        } else if (msg.ok === false && msg.error) {
            lastError = msg.error;
            ToastService.showError(I18n.trFor("quickshare", "Quick Share"), msg.error);
        }
    }

    DCommon.DSocket {
        id: socket

        path: (Quickshell.env("XDG_RUNTIME_DIR") || "/tmp") + "/dms-quickshare.sock"
        connected: root.enabled

        onConnectionStateChanged: {
            if (linkUp) {
                send({
                    "id": 0,
                    "cmd": "subscribe",
                    "ui": true
                });
                root._restarts = 0;
                startTimer.stop();
            } else if (root.enabled) {
                startTimer.restart();
            }
        }

        parser: SplitParser {
            onRead: line => root._handle(line)
        }
    }

    // --- starting the daemon ---

    onEnabledChanged: {
        if (enabled) {
            resolveBinary();
            startTimer.restart();
        } else {
            startTimer.stop();
            restartTimer.stop();
            if (backend.running)
                backend.running = false;
            state = {};
        }
    }

    function resolveBinary(then) {
        Proc.runCommand("quickshare.resolve", ["sh", "-c", "command -v dms-quickshare || { test -x \"$HOME/.local/bin/dms-quickshare\" && echo \"$HOME/.local/bin/dms-quickshare\"; }"], (stdout, exitCode) => {
            const path = stdout.trim().split("\n")[0] || "";
            root.binaryPath = path;
            root.binaryMissing = path === "";
            if (then)
                then();
        });
    }

    // Give an already running daemon a moment before starting our own. The
    // binary is looked up each time, so installing or removing it is noticed.
    Timer {
        id: startTimer

        interval: 1500
        onTriggered: {
            if (!root.enabled || socket.linkUp || backend.running)
                return;
            root.resolveBinary(() => {
                if (!root.enabled || socket.linkUp || backend.running || root.installing)
                    return;
                if (root.binaryMissing) {
                    root._restarts++;
                    restartTimer.restart();
                    return;
                }
                backend.running = true;
            });
        }
    }

    property int _restarts: 0

    Timer {
        id: restartTimer

        interval: Math.min(30000, 2000 * Math.pow(2, Math.min(root._restarts, 4)))
        onTriggered: startTimer.restart()
    }

    Process {
        id: backend

        command: [root.binaryPath, "daemon"]

        onExited: (exitCode, exitStatus) => {
            if (!root.enabled)
                return;
            // Another daemon may own the socket; otherwise try again later.
            root._restarts++;
            if (!socket.linkUp)
                restartTimer.restart();
        }
    }

    // --- setup: the daemon is built from this plugin's own folder ---

    readonly property var pluginInfo: PluginService.availablePlugins["quickshare"] ?? ({})
    readonly property string pluginDir: pluginInfo.pluginDirectory ?? ""
    // plugin.json and daemon/Cargo.toml carry the same version.
    readonly property string pluginVersion: pluginInfo.version ?? ""
    readonly property bool daemonOutdated: connected && version !== "" && pluginVersion !== "" && _older(version, pluginVersion)

    property bool hasCargo: true
    property bool installing: false
    property string installProgress: ""
    property string installError: ""
    property var _installLog: []

    property var wifiSetup: ({})
    property bool helperInstalling: false

    function _older(a, b) {
        const pa = String(a).split(".").map(n => parseInt(n) || 0);
        const pb = String(b).split(".").map(n => parseInt(n) || 0);
        for (let i = 0; i < 3; i++) {
            if ((pa[i] ?? 0) !== (pb[i] ?? 0))
                return (pa[i] ?? 0) < (pb[i] ?? 0);
        }
        return false;
    }

    onBinaryMissingChanged: {
        if (!binaryMissing)
            return;
        checkCargo();
        ToastService.showInfo(I18n.trFor("quickshare", "Quick Share: finish the setup"), I18n.trFor("quickshare", "Open Quick Share in the bar and press Install."), "", "quickshare-setup");
    }

    function checkCargo() {
        Proc.runCommand("quickshare.cargo", ["sh", "-c", "command -v cargo || test -x \"$HOME/.cargo/bin/cargo\""], (stdout, exitCode) => {
            root.hasCargo = exitCode === 0;
        });
    }

    // Builds the daemon with install.sh (no root) and starts it.
    function installDaemon() {
        if (installing || pluginDir === "")
            return;
        installError = "";
        installProgress = "";
        _installLog = [];
        installing = true;
        installer.running = true;
    }

    function _installLine(line) {
        const text = line.trim();
        if (text === "")
            return;
        _installLog = _installLog.concat([text]).slice(-6);
        const step = text.match(/^(Compiling|Downloaded|Building|Installed)\s+(\S+)/);
        if (step)
            installProgress = step[1] === "Compiling" ? I18n.trFor("quickshare", "Compiling %1").arg(step[2]) : text;
    }

    Process {
        id: installer

        command: ["bash", root.pluginDir + "/install.sh"]
        stdout: SplitParser {
            onRead: line => root._installLine(line)
        }
        stderr: SplitParser {
            onRead: line => root._installLine(line)
        }

        onExited: (exitCode, exitStatus) => {
            root.installing = false;
            if (exitCode !== 0) {
                root.installError = root._installLog.filter(l => !l.startsWith("warning:")).slice(-3).join("\n") || I18n.trFor("quickshare", "The build failed.");
                root.checkCargo();
                return;
            }
            // Replace a daemon we started with the new build; otherwise start one.
            root._restarts = 0;
            if (backend.running)
                backend.running = false;
            else
                startTimer.restart();
        }
    }

    function refreshWifiSetup() {
        call("wifiSetup", {}, (ok, result) => {
            if (ok)
                root.wifiSetup = result;
        });
    }

    // Installs the optional root helper; Dux asks for the password.
    function installHelper() {
        if (helperInstalling || pluginDir === "")
            return;
        helperInstalling = true;
        helperInstaller.running = true;
    }

    Process {
        id: helperInstaller

        command: ["pkexec", "bash", root.pluginDir + "/install.sh", "--wifi-helper-only"]

        onExited: (exitCode, exitStatus) => {
            root.helperInstalling = false;
            // 126 and 127: the password dialog was dismissed.
            if (exitCode !== 0 && exitCode !== 126 && exitCode !== 127)
                ToastService.showError(I18n.trFor("quickshare", "Quick Share"), I18n.trFor("quickshare", "Installing the Wi-Fi helper failed."));
            root.refreshWifiSetup();
        }
    }
}
