import QtQuick
import Quickshell
import qs.Common
import qs.Modals.FileBrowser
import qs.Modules.Plugins
import qs.DCommon.Widgets

// Settings page. The settings live in the daemon
// (~/.config/dms-quickshare/config.json), so every control reads the
// daemon's state and writes through it.
PluginSettings {
    id: root

    pluginId: "quickshare"

    readonly property bool ready: QuickShareService.connected
    readonly property var cfg: QuickShareService.config

    component SectionTitle: StyledText {
        width: parent ? parent.width : implicitWidth
        topPadding: Theme.spacingM
        color: Theme.primary
        font.pixelSize: Theme.fontSizeMedium
        font.weight: Theme.fontWeightMedium
    }

    function set(key, value) {
        const patch = {};
        patch[key] = value;
        QuickShareService.setConfig(patch);
    }

    Component.onCompleted: {
        if (ready)
            QuickShareService.refreshWifiSetup();
    }
    onReadyChanged: {
        if (ready)
            QuickShareService.refreshWifiSetup();
    }

    QsSetupCard {
        width: parent.width
    }

    StyledText {
        visible: !root.ready && !QuickShareService.binaryMissing && !QuickShareService.installing
        width: parent.width
        text: I18n.trFor("quickshare", "Quick Share is not running. Enable the plugin to change its settings.")
        color: Theme.surfaceVariantText
        font.pixelSize: Theme.fontSizeMedium
        wrapMode: Text.Wrap
    }

    // --- this device ---

    SectionTitle {
        visible: root.ready
        text: I18n.trFor("quickshare", "This device")
    }

    DTextField {
        visible: root.ready
        width: parent.width
        labelText: I18n.trFor("quickshare", "Device name")
        supportingText: I18n.trFor("quickshare", "How nearby phones see this PC. Press Enter to save.")
        maximumLength: 64
        text: QuickShareService.deviceName
        onAccepted: {
            if (text.trim().length > 0)
                QuickShareService.setName(text.trim());
        }
    }

    StyledText {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Visibility")
        color: Theme.surfaceText
        font.pixelSize: Theme.fontSizeMedium
    }

    QsVisibilityPicker {
        visible: root.ready
        width: parent.width
    }

    StyledText {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Temporary visibility lasts")
        color: Theme.surfaceText
        font.pixelSize: Theme.fontSizeMedium
    }

    DButtonGroup {
        id: durationPicker

        readonly property var choices: [5, 10, 30, 60]

        visible: root.ready
        size: "small"
        model: choices.map(m => I18n.trFor("quickshare", "%1 min").arg(m))
        currentIndex: choices.indexOf(root.cfg.temporaryMinutes ?? 10)
        onSelectionChanged: (index, selected) => {
            if (selected && choices[index] !== root.cfg.temporaryMinutes)
                root.set("temporaryMinutes", choices[index]);
        }
    }

    // --- receiving ---

    SectionTitle {
        visible: root.ready
        text: I18n.trFor("quickshare", "Receiving")
    }

    Item {
        visible: root.ready
        width: parent.width
        implicitHeight: Math.max(folderText.implicitHeight, folderButtons.implicitHeight)

        Column {
            id: folderText

            anchors.left: parent.left
            anchors.right: folderButtons.left
            anchors.verticalCenter: parent.verticalCenter
            anchors.rightMargin: Theme.spacingS
            spacing: Theme.spacingXS

            StyledText {
                width: parent.width
                text: I18n.trFor("quickshare", "Save received files to")
                color: Theme.surfaceText
                font.pixelSize: Theme.fontSizeMedium
            }

            StyledText {
                width: parent.width
                text: QuickShareService.downloadDir
                color: Theme.surfaceVariantText
                font.pixelSize: Theme.fontSizeSmall
                elide: Text.ElideMiddle
                wrapMode: Text.NoWrap
            }
        }

        Row {
            id: folderButtons

            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            spacing: Theme.spacingXS

            DButton {
                text: I18n.trFor("quickshare", "Open")
                variant: "text"
                onClicked: Qt.openUrlExternally("file://" + QuickShareService.downloadDir)
            }

            DButton {
                text: I18n.trFor("quickshare", "Change")
                variant: "tonal"
                onClicked: {
                    folderLoader.active = true;
                    if (folderLoader.item)
                        folderLoader.item.open();
                }
            }
        }
    }

    DToggle {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Accept files without asking")
        description: I18n.trFor("quickshare", "Anyone nearby who can see this PC could then send you files.")
        checked: root.cfg.autoAccept === true
        onToggled: checked => root.set("autoAccept", checked)
    }

    DToggle {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Notifications")
        description: I18n.trFor("quickshare", "Ask for incoming transfers and report finished ones.")
        checked: root.cfg.notifications !== false
        onToggled: checked => root.set("notifications", checked)
    }

    // --- connections ---

    SectionTitle {
        visible: root.ready
        text: I18n.trFor("quickshare", "Connections")
    }

    DToggle {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Bluetooth")
        description: I18n.trFor("quickshare", "Find phones and connect to them; needed when they are on another network.")
        checked: root.cfg.useBluetooth !== false
        onToggled: checked => root.set("useBluetooth", checked)
    }

    DToggle {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Switch Bluetooth on at start")
        description: I18n.trFor("quickshare", "When visible. Never overrides airplane mode.")
        checked: root.cfg.autoEnableBluetooth !== false
        onToggled: checked => root.set("autoEnableBluetooth", checked)
    }

    DToggle {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Local network")
        description: I18n.trFor("quickshare", "Find and reach phones on the same Wi-Fi or Ethernet network.")
        checked: root.cfg.useWifi !== false
        onToggled: checked => root.set("useWifi", checked)
    }

    DToggle {
        visible: root.ready
        width: parent.width
        text: I18n.trFor("quickshare", "Fast transfers over Wi-Fi")
        description: I18n.trFor("quickshare", "Move transfers from Bluetooth to a direct Wi-Fi link, also between different networks. Your own Wi-Fi connection stays as it is.")
        checked: root.cfg.wifiUpgrade !== false
        onToggled: checked => root.set("wifiUpgrade", checked)
    }

    DToggle {
        visible: root.ready
        width: parent.width
        enabled: root.cfg.wifiUpgrade !== false
        text: I18n.trFor("quickshare", "Allow a short Wi-Fi interruption")
        description: I18n.trFor("quickshare", "For Wi-Fi cards that cannot run a second link: briefly leave your network during a transfer, then reconnect.")
        checked: root.cfg.wifiInterrupt === true
        onToggled: checked => root.set("wifiInterrupt", checked)
    }

    // Only offered where the helper would make fast transfers possible.
    Item {
        visible: root.ready && QuickShareService.wifiSetup.helperRecommended === true
        width: parent.width
        implicitHeight: Math.max(helperText.implicitHeight, helperButton.implicitHeight)

        Column {
            id: helperText

            anchors.left: parent.left
            anchors.right: helperButton.left
            anchors.verticalCenter: parent.verticalCenter
            anchors.rightMargin: Theme.spacingS
            spacing: Theme.spacingXS

            StyledText {
                width: parent.width
                text: I18n.trFor("quickshare", "Wi-Fi helper")
                color: Theme.surfaceText
                font.pixelSize: Theme.fontSizeMedium
            }

            StyledText {
                width: parent.width
                text: I18n.trFor("quickshare", "Without a second Wi-Fi adapter, this PC needs the helper for fast transfers while the phone is on another network. It runs a temporary link next to your Wi-Fi connection. Installing asks for your password once.")
                color: Theme.surfaceVariantText
                font.pixelSize: Theme.fontSizeSmall
                wrapMode: Text.Wrap
            }
        }

        DButton {
            id: helperButton

            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            enabled: !QuickShareService.helperInstalling
            text: QuickShareService.helperInstalling ? I18n.trFor("quickshare", "Installing…") : I18n.trFor("quickshare", "Install")
            variant: "tonal"
            onClicked: QuickShareService.installHelper()
        }
    }

    StyledText {
        visible: root.ready
        width: parent.width
        topPadding: Theme.spacingM
        text: I18n.trFor("quickshare", "Daemon %1. For a full check of your Bluetooth and Wi-Fi setup run: dms-quickshare doctor").arg(QuickShareService.version)
        color: Theme.surfaceVariantText
        font.pixelSize: Theme.fontSizeSmall
        wrapMode: Text.Wrap
    }

    LazyLoader {
        id: folderLoader

        active: false

        FileBrowserSurfaceModal {
            browserTitle: I18n.trFor("quickshare", "Folder for received files")
            folderMode: true
            onAccepted: paths => {
                if (paths.length > 0)
                    root.set("downloadDir", paths[0]);
            }
        }
    }
}
