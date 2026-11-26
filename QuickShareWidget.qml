import QtQuick
import qs.Common
import qs.DCommon.Widgets
import qs.Modules.Plugins

// Bar/dock pill, popout and Control Center tile.
PluginComponent {
    id: root

    property var popoutService: null

    layerNamespacePlugin: "quickshare"
    popoutWidth: 400
    popoutHeight: 560

    readonly property bool hasRequest: QuickShareService.requests.length > 0
    readonly property var current: QuickShareService.activeTransfers.find(t => t.state === "transferring") ?? null
    readonly property string pillIcon: {
        if (!QuickShareService.connected)
            return "share";
        if (hasRequest)
            return "download";
        if (current)
            return current.direction === "incoming" ? "download" : "upload";
        return QuickShareService.isVisible ? "share" : "visibility_off";
    }
    readonly property color pillColor: hasRequest || current ? Theme.primary : Theme.widgetIconColor
    readonly property string pillText: current ? Math.round(QuickShareService.progress(current) * 100) + "%" : ""

    // Files dropped on the pill or handed over by the CLI: open the popout
    // so the user picks the device.
    function offerPendingFiles() {
        if (QuickShareService.pendingFiles.length > 0 && !root.interactionActive)
            root.triggerPopout();
    }

    function dropFiles(urls) {
        QuickShareService.setPendingFiles(urls);
        offerPendingFiles();
    }

    // Takes files dragged from a file manager. The drop must be accepted
    // explicitly, or the Wayland drag is cancelled before it is dropped.
    component PillDropArea: DropArea {
        signal filesDropped(var urls)

        anchors.fill: parent
        anchors.margins: -Theme.spacingS
        keys: ["text/uri-list"]
        onEntered: drag => drag.accept(Qt.CopyAction)
        onDropped: drop => {
            drop.accept(Qt.CopyAction);
            filesDropped(drop.urls);
        }
    }

    Connections {
        target: QuickShareService

        function onSendRequested(files) {
            root.offerPendingFiles();
        }
    }

    horizontalBarPill: Component {
        Item {
            implicitWidth: pillRow.implicitWidth
            implicitHeight: pillRow.implicitHeight

            Row {
                id: pillRow

                spacing: Theme.spacingXS

                DIcon {
                    anchors.verticalCenter: parent.verticalCenter
                    name: hDrop.containsDrag ? "upload" : root.pillIcon
                    size: root.iconSize
                    color: hDrop.containsDrag ? Theme.primary : root.pillColor
                    filled: root.hasRequest
                    scale: hDrop.containsDrag ? 1.2 : 1
                }

                StyledText {
                    anchors.verticalCenter: parent.verticalCenter
                    visible: root.pillText !== ""
                    text: root.pillText
                    color: Theme.widgetTextColor
                    font.pixelSize: root.textSize
                }
            }

            PillDropArea {
                id: hDrop

                onFilesDropped: urls => root.dropFiles(urls)
            }
        }
    }

    verticalBarPill: Component {
        Item {
            implicitWidth: pillColumn.implicitWidth
            implicitHeight: pillColumn.implicitHeight

            Column {
                id: pillColumn

                spacing: Theme.spacingXS

                DIcon {
                    anchors.horizontalCenter: parent.horizontalCenter
                    name: vDrop.containsDrag ? "upload" : root.pillIcon
                    size: root.iconSize
                    color: vDrop.containsDrag ? Theme.primary : root.pillColor
                    filled: root.hasRequest
                    scale: vDrop.containsDrag ? 1.2 : 1
                }

                StyledText {
                    anchors.horizontalCenter: parent.horizontalCenter
                    visible: root.pillText !== ""
                    text: root.pillText
                    color: Theme.widgetTextColor
                    font.pixelSize: root.textSize
                }
            }

            PillDropArea {
                id: vDrop

                onFilesDropped: urls => root.dropFiles(urls)
            }
        }
    }

    popoutContent: Component {
        PopoutComponent {
            id: popout

            headerText: I18n.trFor("quickshare", "Quick Share")
            detailsText: QuickShareService.connected ? QuickShareService.visibilityLabel() : ""

            QuickSharePopout {
                width: parent.width
                height: root.popoutHeight - popout.headerHeight - popout.detailsHeight - Theme.spacingXL
                parentPopout: popout.parentPopout
            }
        }
    }

    // Control Center: toggles visibility like the Android quick setting.
    ccWidgetIcon: QuickShareService.isVisible ? "share" : "visibility_off"
    ccWidgetPrimaryText: I18n.trFor("quickshare", "Quick Share")
    ccWidgetSecondaryText: QuickShareService.connected ? QuickShareService.visibilityLabel() : I18n.trFor("quickshare", "Not running")
    ccWidgetIsActive: QuickShareService.isVisible
    onCcWidgetToggled: QuickShareService.setVisibility(QuickShareService.isVisible ? "hidden" : "everyone")
}
