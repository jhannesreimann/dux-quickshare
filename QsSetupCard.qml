import QtQuick
import qs.Common
import qs.DCommon.Widgets

// Shown until the daemon is installed, and after a plugin update until it
// is rebuilt. It is built from the plugin folder by install.sh.
StyledRect {
    id: card

    readonly property bool missing: QuickShareService.binaryMissing
    readonly property bool busy: QuickShareService.installing

    visible: missing || busy || QuickShareService.daemonOutdated
    implicitHeight: column.implicitHeight + Theme.spacingM * 2
    radius: Theme.cornerRadius
    color: Theme.cardSurface

    // Opening the panel or settings notices a daemon installed meanwhile.
    Component.onCompleted: QuickShareService.recheckBinary()

    Column {
        id: column

        anchors.left: parent.left
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.margins: Theme.spacingM
        spacing: Theme.spacingS

        StyledText {
            width: parent.width
            text: card.missing ? I18n.trFor("quickshare", "Finish setting up Quick Share") : I18n.trFor("quickshare", "Update the Quick Share service")
            color: Theme.surfaceText
            font.pixelSize: Theme.fontSizeMedium
            font.weight: Theme.fontWeightMedium
        }

        StyledText {
            width: parent.width
            text: card.missing ? I18n.trFor("quickshare", "Quick Share needs its background service. It is built from this plugin's source code on your PC and installed to ~/.local/bin, without root access. The first build takes a few minutes.") : I18n.trFor("quickshare", "The plugin was updated. Rebuild the service (version %1) to match the plugin (version %2).").arg(QuickShareService.version).arg(QuickShareService.pluginVersion)
            color: Theme.surfaceVariantText
            font.pixelSize: Theme.fontSizeSmall
            wrapMode: Text.Wrap
        }

        StyledText {
            width: parent.width
            visible: card.missing && !card.busy && !QuickShareService.hasCargo
            text: I18n.trFor("quickshare", "Building needs Rust 1.89 or newer. Install it with your package manager (Arch: sudo pacman -S rust) or from rustup.rs, then press Install.")
            color: Theme.primary
            font.pixelSize: Theme.fontSizeSmall
            wrapMode: Text.Wrap
        }

        Row {
            width: parent.width
            visible: card.busy
            spacing: Theme.spacingS

            DLoadingIndicator {
                anchors.verticalCenter: parent.verticalCenter
                size: Theme.iconSize
                running: card.busy
            }

            StyledText {
                anchors.verticalCenter: parent.verticalCenter
                width: parent.width - Theme.iconSize - parent.spacing
                text: QuickShareService.installProgress || I18n.trFor("quickshare", "Building…")
                color: Theme.surfaceVariantText
                font.pixelSize: Theme.fontSizeSmall
                elide: Text.ElideRight
                wrapMode: Text.NoWrap
            }
        }

        StyledText {
            width: parent.width
            visible: !card.busy && QuickShareService.installError !== ""
            text: QuickShareService.installError
            color: Theme.error
            font.pixelSize: Theme.fontSizeSmall
            wrapMode: Text.Wrap
            maximumLineCount: 4
            elide: Text.ElideRight
        }

        DButton {
            anchors.right: parent.right
            visible: !card.busy
            text: card.missing ? I18n.trFor("quickshare", "Install") : I18n.trFor("quickshare", "Update")
            iconName: "download"
            onClicked: QuickShareService.installDaemon()
        }
    }
}
