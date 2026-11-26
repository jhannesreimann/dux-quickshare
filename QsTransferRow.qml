import QtQuick
import qs.Common
import qs.DCommon.Widgets

// A running or finished transfer.
StyledRect {
    id: row

    property var transfer: ({})

    readonly property bool active: ["connecting", "awaiting_acceptance", "transferring"].includes(transfer.state)
    readonly property bool succeeded: transfer.state === "completed"
    readonly property bool failed: transfer.state === "failed"
    readonly property bool canOpen: succeeded && transfer.direction === "incoming" && ((transfer.savedPaths ?? []).length > 0 || !!transfer.text)
    readonly property real progress: QuickShareService.progress(transfer)

    implicitHeight: column.implicitHeight + Theme.spacingM * 2
    radius: Theme.cornerRadius
    color: mouse.containsMouse && canOpen ? Theme.chipSurface : Theme.cardSurface

    MouseArea {
        id: mouse

        anchors.fill: parent
        hoverEnabled: true
        enabled: row.canOpen
        cursorShape: row.canOpen ? Qt.PointingHandCursor : Qt.ArrowCursor
        onClicked: QuickShareService.open(row.transfer.id, false)
    }

    Column {
        id: column

        anchors.left: parent.left
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.leftMargin: Theme.spacingM
        anchors.rightMargin: Theme.spacingS
        spacing: Theme.spacingS

        Row {
            width: parent.width
            spacing: Theme.spacingM

            DIcon {
                anchors.verticalCenter: parent.verticalCenter
                name: row.failed ? "error" : QuickShareService.itemIcon(row.transfer)
                size: Theme.iconSize
                color: row.failed ? Theme.error : Theme.primary
            }

            Column {
                anchors.verticalCenter: parent.verticalCenter
                width: parent.width - Theme.iconSize - actions.width - parent.spacing * 2
                spacing: Theme.spacingXS

                StyledText {
                    width: parent.width
                    text: QuickShareService.summary(row.transfer)
                    color: Theme.surfaceText
                    font.pixelSize: Theme.fontSizeMedium
                    font.weight: Theme.fontWeightMedium
                    elide: Text.ElideMiddle
                    wrapMode: Text.NoWrap
                }

                StyledText {
                    width: parent.width
                    text: {
                        const t = row.transfer;
                        const parts = [t.direction === "incoming" ? I18n.trFor("quickshare", "From %1").arg(t.peerName) : I18n.trFor("quickshare", "To %1").arg(t.peerName), QuickShareService.stateLabel(t)];
                        if (t.state === "transferring") {
                            parts.push(QuickShareService.formatBytes(t.bytesDone) + " / " + QuickShareService.formatBytes(t.totalBytes));
                            const medium = QuickShareService.mediumLabel(t.medium);
                            if (medium)
                                parts.push(medium);
                        }
                        return parts.join(" · ");
                    }
                    color: Theme.surfaceVariantText
                    font.pixelSize: Theme.fontSizeSmall
                    elide: Text.ElideRight
                    wrapMode: Text.NoWrap
                }

                // The verification code the receiving phone shows too.
                StyledText {
                    width: parent.width
                    visible: row.active && row.transfer.direction === "outgoing" && !!row.transfer.pin
                    text: I18n.trFor("quickshare", "Code %1: check that %2 shows the same").arg(row.transfer.pin ?? "").arg(row.transfer.peerName ?? "")
                    color: Theme.primary
                    font.pixelSize: Theme.fontSizeSmall
                    font.weight: Theme.fontWeightMedium
                    wrapMode: Text.Wrap
                }

                StyledText {
                    width: parent.width
                    visible: row.failed && !!row.transfer.error
                    text: row.transfer.error ?? ""
                    color: Theme.error
                    font.pixelSize: Theme.fontSizeSmall
                    wrapMode: Text.Wrap
                    maximumLineCount: 3
                    elide: Text.ElideRight
                }
            }

            Row {
                id: actions

                anchors.verticalCenter: parent.verticalCenter
                spacing: Theme.spacingXS

                DIconButton {
                    visible: row.canOpen && (row.transfer.savedPaths ?? []).length > 0
                    iconName: "folder_open"
                    tooltipText: I18n.trFor("quickshare", "Show in folder")
                    onClicked: QuickShareService.open(row.transfer.id, true)
                }

                DIconButton {
                    iconName: "close"
                    tooltipText: row.active ? I18n.trFor("quickshare", "Cancel") : I18n.trFor("quickshare", "Remove from list")
                    onClicked: row.active ? QuickShareService.cancel(row.transfer.id) : QuickShareService.dismiss(row.transfer.id)
                }
            }
        }

        // Progress bar while data flows.
        StyledRect {
            visible: row.transfer.state === "transferring"
            width: parent.width - Theme.spacingS
            height: Theme.spacingXS
            radius: height / 2
            color: Theme.withAlpha(Theme.primary, 0.2)

            StyledRect {
                width: parent.width * row.progress
                height: parent.height
                radius: parent.radius
                color: Theme.primary

                Behavior on width {
                    NumberAnimation {
                        duration: Theme.shortDuration
                    }
                }
            }
        }
    }
}
