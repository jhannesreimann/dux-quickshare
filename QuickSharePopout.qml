pragma ComponentBehavior: Bound

import QtQuick
import Quickshell
import qs.Common
import qs.Modals.FileBrowser
import qs.DCommon.Widgets

// Content of the bar widget's popout.
Item {
    id: root

    property var parentPopout: null

    readonly property bool ready: QuickShareService.connected
    // Device to send to once files are picked ("" = only select files).
    property string pickerTarget: ""
    property bool editingName: false

    component SectionTitle: StyledText {
        width: parent ? parent.width : implicitWidth
        color: Theme.primary
        font.pixelSize: Theme.fontSizeSmall
        font.weight: Theme.fontWeightMedium
    }

    function pickFiles(deviceId) {
        root.pickerTarget = deviceId;
        pickerLoader.active = true;
        if (pickerLoader.item)
            pickerLoader.item.open();
    }

    function sendTo(device) {
        if (QuickShareService.pendingFiles.length > 0) {
            QuickShareService.sendFiles(device.id, QuickShareService.pendingFiles);
            QuickShareService.pendingFiles = [];
        } else {
            pickFiles(device.id);
        }
    }

    // Look for nearby devices while the popout is open.
    function refreshDiscovery() {
        if (root.ready)
            QuickShareService.startDiscovery(120);
    }

    Component.onCompleted: refreshDiscovery()
    Component.onDestruction: {
        if (root.ready)
            QuickShareService.stopDiscovery();
    }
    onReadyChanged: refreshDiscovery()

    Timer {
        interval: 100000
        repeat: true
        running: root.ready && root.visible
        onTriggered: root.refreshDiscovery()
    }

    DropArea {
        anchors.fill: parent
        keys: ["text/uri-list"]
        onEntered: drag => drag.accept(Qt.CopyAction)
        onDropped: drop => {
            drop.accept(Qt.CopyAction);
            QuickShareService.setPendingFiles(drop.urls);
        }
    }

    DFlickable {
        id: scroller

        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true

        Column {
            id: column

            width: scroller.width
            spacing: Theme.spacingL

            // --- daemon missing, outdated or starting ---
            QsSetupCard {
                width: parent.width
            }

            StyledRect {
                visible: !root.ready && !QuickShareService.binaryMissing && !QuickShareService.installing
                width: parent.width
                implicitHeight: starting.implicitHeight + Theme.spacingM * 2
                radius: Theme.cornerRadius
                color: Theme.cardSurface

                StyledText {
                    id: starting

                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.margins: Theme.spacingM
                    text: I18n.trFor("quickshare", "Starting Quick Share…")
                    color: Theme.surfaceText
                    font.pixelSize: Theme.fontSizeMedium
                    font.weight: Theme.fontWeightMedium
                }
            }

            // --- Bluetooth off ---
            StyledRect {
                visible: root.ready && !QuickShareService.bluetoothOn
                width: parent.width
                implicitHeight: btRow.implicitHeight + Theme.spacingM * 2
                radius: Theme.cornerRadius
                color: Theme.cardSurface

                Row {
                    id: btRow

                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.margins: Theme.spacingM
                    spacing: Theme.spacingM

                    DIcon {
                        anchors.verticalCenter: parent.verticalCenter
                        name: "bluetooth_disabled"
                        size: Theme.iconSize
                        color: Theme.surfaceVariantText
                    }

                    Column {
                        anchors.verticalCenter: parent.verticalCenter
                        width: parent.width - Theme.iconSize - btButton.width - parent.spacing * 2
                        spacing: Theme.spacingXS

                        StyledText {
                            width: parent.width
                            text: I18n.trFor("quickshare", "Bluetooth is off")
                            color: Theme.surfaceText
                            font.pixelSize: Theme.fontSizeMedium
                            font.weight: Theme.fontWeightMedium
                        }

                        StyledText {
                            width: parent.width
                            text: I18n.trFor("quickshare", "Phones find this PC over Bluetooth.")
                            color: Theme.surfaceVariantText
                            font.pixelSize: Theme.fontSizeSmall
                            wrapMode: Text.Wrap
                        }
                    }

                    DButton {
                        id: btButton

                        anchors.verticalCenter: parent.verticalCenter
                        text: I18n.trFor("quickshare", "Turn on")
                        variant: "tonal"
                        onClicked: QuickShareService.enableBluetooth()
                    }
                }
            }

            // --- this device: name and visibility ---
            Column {
                visible: root.ready
                width: parent.width
                spacing: Theme.spacingS

                Item {
                    width: parent.width
                    implicitHeight: Math.max(nameLabel.implicitHeight, nameField.implicitHeight, editButton.implicitHeight)

                    StyledText {
                        id: nameLabel

                        visible: !root.editingName
                        anchors.left: parent.left
                        anchors.right: editButton.left
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.rightMargin: Theme.spacingS
                        text: I18n.trFor("quickshare", "Shown as %1").arg(QuickShareService.deviceName)
                        color: Theme.surfaceText
                        font.pixelSize: Theme.fontSizeMedium
                        elide: Text.ElideRight
                        wrapMode: Text.NoWrap
                    }

                    DTextField {
                        id: nameField

                        visible: root.editingName
                        anchors.left: parent.left
                        anchors.right: editButton.left
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.rightMargin: Theme.spacingS
                        labelText: I18n.trFor("quickshare", "Device name")
                        maximumLength: 64
                        onAccepted: {
                            if (text.trim().length > 0)
                                QuickShareService.setName(text.trim());
                            root.editingName = false;
                        }
                    }

                    DIconButton {
                        id: editButton

                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        iconName: root.editingName ? "check" : "edit"
                        tooltipText: root.editingName ? I18n.trFor("quickshare", "Save name") : I18n.trFor("quickshare", "Change name")
                        onClicked: {
                            if (root.editingName) {
                                nameField.accepted();
                                return;
                            }
                            nameField.text = QuickShareService.deviceName;
                            root.editingName = true;
                            Qt.callLater(() => {
                                nameField.forceActiveFocus();
                                nameField.selectAll();
                            });
                        }
                    }
                }

                QsVisibilityPicker {
                    width: parent.width
                }

                StyledText {
                    width: parent.width
                    text: QuickShareService.visibilityLabel()
                    color: Theme.surfaceVariantText
                    font.pixelSize: Theme.fontSizeSmall
                }
            }

            // --- incoming requests ---
            Column {
                visible: QuickShareService.requests.length > 0
                width: parent.width
                spacing: Theme.spacingS

                Repeater {
                    model: QuickShareService.requests

                    QsRequestCard {
                        required property var modelData

                        width: parent.width
                        transfer: modelData
                    }
                }
            }

            // --- running transfers ---
            Column {
                visible: QuickShareService.activeTransfers.length > 0
                width: parent.width
                spacing: Theme.spacingS

                SectionTitle {
                    text: I18n.trFor("quickshare", "Transfers")
                }

                Repeater {
                    model: QuickShareService.activeTransfers

                    QsTransferRow {
                        required property var modelData

                        width: parent.width
                        transfer: modelData
                    }
                }
            }

            // --- send ---
            Column {
                visible: root.ready
                width: parent.width
                spacing: Theme.spacingS

                Item {
                    width: parent.width
                    implicitHeight: Math.max(sendTitle.implicitHeight, chooseButton.implicitHeight)

                    SectionTitle {
                        id: sendTitle

                        anchors.left: parent.left
                        anchors.right: chooseButton.left
                        anchors.verticalCenter: parent.verticalCenter
                        text: QuickShareService.pendingFiles.length > 0 ? I18n.trFor("quickshare", "Send %1 file(s) to").arg(QuickShareService.pendingFiles.length) : I18n.trFor("quickshare", "Nearby devices")
                    }

                    DButton {
                        id: chooseButton

                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        text: QuickShareService.pendingFiles.length > 0 ? I18n.trFor("quickshare", "Clear") : I18n.trFor("quickshare", "Choose files")
                        iconName: QuickShareService.pendingFiles.length > 0 ? "close" : "attach_file"
                        variant: "text"
                        onClicked: {
                            if (QuickShareService.pendingFiles.length > 0)
                                QuickShareService.pendingFiles = [];
                            else
                                root.pickFiles("");
                        }
                    }
                }

                Repeater {
                    model: QuickShareService.devices

                    QsDeviceRow {
                        required property var modelData

                        width: parent.width
                        device: modelData
                        onClicked: root.sendTo(modelData)
                    }
                }

                Row {
                    visible: QuickShareService.devices.length === 0
                    width: parent.width
                    spacing: Theme.spacingM

                    DLoadingIndicator {
                        anchors.verticalCenter: parent.verticalCenter
                        size: Theme.iconSizeLarge
                        running: visible && QuickShareService.discovering
                    }

                    StyledText {
                        anchors.verticalCenter: parent.verticalCenter
                        width: parent.width - Theme.iconSizeLarge - parent.spacing
                        text: I18n.trFor("quickshare", "Looking for nearby devices. On the other phone, open Quick Share and make it visible.")
                        color: Theme.surfaceVariantText
                        font.pixelSize: Theme.fontSizeSmall
                        wrapMode: Text.Wrap
                    }
                }
            }

            // --- recent transfers ---
            Column {
                visible: QuickShareService.finishedTransfers.length > 0
                width: parent.width
                spacing: Theme.spacingS

                Item {
                    width: parent.width
                    implicitHeight: Math.max(recentTitle.implicitHeight, clearButton.implicitHeight)

                    SectionTitle {
                        id: recentTitle

                        anchors.left: parent.left
                        anchors.right: clearButton.left
                        anchors.verticalCenter: parent.verticalCenter
                        text: I18n.trFor("quickshare", "Recent")
                    }

                    DButton {
                        id: clearButton

                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        text: I18n.trFor("quickshare", "Clear")
                        variant: "text"
                        onClicked: QuickShareService.clearFinished()
                    }
                }

                Repeater {
                    model: QuickShareService.finishedTransfers.slice(0, 5)

                    QsTransferRow {
                        required property var modelData

                        width: parent.width
                        transfer: modelData
                    }
                }
            }
        }
    }

    LazyLoader {
        id: pickerLoader

        active: false

        FileBrowserSurfaceModal {
            browserTitle: root.pickerTarget ? I18n.trFor("quickshare", "Choose files to send") : I18n.trFor("quickshare", "Choose files")
            multiple: true
            parentPopout: root.parentPopout
            onAccepted: paths => {
                if (root.pickerTarget)
                    QuickShareService.sendFiles(root.pickerTarget, paths);
                else
                    QuickShareService.setPendingFiles(paths);
                root.pickerTarget = "";
            }
            onRejected: root.pickerTarget = ""
        }
    }
}
