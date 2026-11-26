import QtQuick
import Quickshell.Io
import qs.Common
import qs.Modules.Plugins

// Background surface: keeps the connection to the dms-quickshare daemon
// alive while the plugin is enabled and exposes IPC commands:
//
//   dms ipc call quickshare status
//   dms ipc call quickshare visibility everyone|temporary|hidden
//   dms ipc call quickshare toggle
//   dms ipc call quickshare send "/path/one\n/path/two"
PluginComponent {
    id: root

    property var popoutService: null

    Component.onCompleted: QuickShareService.enabled = true
    Component.onDestruction: QuickShareService.enabled = false

    IpcHandler {
        target: "quickshare"

        function status(): string {
            if (!QuickShareService.connected)
                return "not running";
            return JSON.stringify({
                "name": QuickShareService.deviceName,
                "visibility": QuickShareService.visibility,
                "visible": QuickShareService.isVisible,
                "bluetooth": QuickShareService.bluetoothOn,
                "devices": QuickShareService.devices.map(d => d.name),
                "active": QuickShareService.activeTransfers.length
            });
        }

        function visibility(mode: string): string {
            if (!["everyone", "temporary", "hidden"].includes(mode))
                return "use everyone, temporary or hidden";
            QuickShareService.setVisibility(mode);
            return "ok";
        }

        function toggle(): string {
            QuickShareService.setVisibility(QuickShareService.isVisible ? "hidden" : "everyone");
            return QuickShareService.isVisible ? "hidden" : "everyone";
        }

        // Newline-separated paths; the bar widget asks for the target device.
        function send(paths: string): string {
            const files = paths.split("\n").map(p => p.trim()).filter(p => p.length > 0);
            if (files.length === 0)
                return "no files given";
            QuickShareService.setPendingFiles(files);
            QuickShareService.sendRequested(QuickShareService.pendingFiles);
            return "ok";
        }
    }
}
