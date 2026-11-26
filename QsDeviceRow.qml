import QtQuick
import qs.Common
import qs.DCommon.Widgets

// A nearby device; click to send to it.
StyledRect {
    id: row

    property var device: ({})

    signal clicked

    readonly property bool sameNetwork: (device.mediums ?? []).includes("wifi")

    implicitHeight: content.implicitHeight + Theme.spacingM * 2
    radius: Theme.cornerRadius
    color: mouse.containsMouse ? Theme.chipSurface : Theme.cardSurface

    Row {
        id: content

        anchors.left: parent.left
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.leftMargin: Theme.spacingM
        anchors.rightMargin: Theme.spacingM
        spacing: Theme.spacingM

        DIcon {
            anchors.verticalCenter: parent.verticalCenter
            name: QuickShareService.deviceIcon(row.device.type)
            size: Theme.iconSize
            color: Theme.primary
        }

        Column {
            anchors.verticalCenter: parent.verticalCenter
            width: parent.width - Theme.iconSize - Theme.iconSizeSmall - parent.spacing * 2
            spacing: Theme.spacingXS

            StyledText {
                width: parent.width
                text: row.device.name ?? ""
                color: Theme.surfaceText
                font.pixelSize: Theme.fontSizeMedium
                font.weight: Theme.fontWeightMedium
                elide: Text.ElideRight
                wrapMode: Text.NoWrap
            }

            StyledText {
                width: parent.width
                text: row.sameNetwork ? I18n.trFor("quickshare", "On your network") : I18n.trFor("quickshare", "Nearby")
                color: Theme.surfaceVariantText
                font.pixelSize: Theme.fontSizeSmall
                elide: Text.ElideRight
                wrapMode: Text.NoWrap
            }
        }

        DIcon {
            anchors.verticalCenter: parent.verticalCenter
            name: "send"
            size: Theme.iconSizeSmall
            color: Theme.surfaceVariantText
        }
    }

    MouseArea {
        id: mouse

        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: row.clicked()
    }
}
