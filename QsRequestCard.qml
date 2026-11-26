import QtQuick
import qs.Common
import qs.DCommon.Widgets

// An incoming transfer waiting for the user's decision.
StyledRect {
    id: card

    property var transfer: ({})

    implicitHeight: column.implicitHeight + Theme.spacingM * 2
    radius: Theme.cornerRadius
    color: Theme.primaryContainer

    Column {
        id: column

        anchors.left: parent.left
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.margins: Theme.spacingM
        spacing: Theme.spacingS

        Row {
            width: parent.width
            spacing: Theme.spacingM

            DIcon {
                anchors.verticalCenter: parent.verticalCenter
                name: QuickShareService.deviceIcon(card.transfer.peerType)
                size: Theme.iconSize
                color: Theme.onPrimaryContainer
            }

            Column {
                anchors.verticalCenter: parent.verticalCenter
                width: parent.width - Theme.iconSize - parent.spacing
                spacing: Theme.spacingXS

                StyledText {
                    width: parent.width
                    text: I18n.trFor("quickshare", "%1 wants to share").arg(card.transfer.peerName ?? "")
                    color: Theme.onPrimaryContainer
                    font.pixelSize: Theme.fontSizeMedium
                    font.weight: Theme.fontWeightMedium
                    elide: Text.ElideRight
                    wrapMode: Text.NoWrap
                }

                StyledText {
                    width: parent.width
                    text: {
                        const t = card.transfer;
                        const size = (t.totalBytes ?? 0) > 0 ? " (" + QuickShareService.formatBytes(t.totalBytes) + ")" : "";
                        return QuickShareService.summary(t) + size;
                    }
                    color: Theme.onPrimaryContainer
                    font.pixelSize: Theme.fontSizeSmall
                    elide: Text.ElideMiddle
                    wrapMode: Text.NoWrap
                }

                StyledText {
                    width: parent.width
                    visible: !!card.transfer.pin
                    text: I18n.trFor("quickshare", "Code %1: check that %2 shows the same").arg(card.transfer.pin ?? "").arg(card.transfer.peerName ?? "")
                    color: Theme.onPrimaryContainer
                    font.pixelSize: Theme.fontSizeSmall
                    wrapMode: Text.Wrap
                }
            }
        }

        Row {
            anchors.right: parent.right
            spacing: Theme.spacingS

            DButton {
                text: I18n.trFor("quickshare", "Decline")
                variant: "text"
                onClicked: QuickShareService.reject(card.transfer.id)
            }

            DButton {
                text: I18n.trFor("quickshare", "Accept")
                iconName: "download"
                onClicked: QuickShareService.accept(card.transfer.id)
            }
        }
    }
}
