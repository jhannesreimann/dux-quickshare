import QtQuick
import qs.Common
import qs.DCommon.Widgets

// Who can see this PC: everyone, everyone for a while, or nobody.
DButtonGroup {
    id: picker

    readonly property var modes: ["everyone", "temporary", "hidden"]
    readonly property int minutes: QuickShareService.config.temporaryMinutes ?? 10

    model: [
        {
            "text": I18n.trFor("quickshare", "Everyone"),
            "icon": "public"
        },
        {
            "text": I18n.trFor("quickshare", "%1 min").arg(minutes),
            "icon": "timer",
            "tooltip": I18n.trFor("quickshare", "Visible to everyone for %1 minutes").arg(minutes)
        },
        {
            "text": I18n.trFor("quickshare", "Hidden"),
            "icon": "visibility_off",
            "tooltip": I18n.trFor("quickshare", "Not visible; you can still send")
        }
    ]

    function sync() {
        currentIndex = Math.max(0, modes.indexOf(QuickShareService.visibility));
    }

    Component.onCompleted: sync()
    onSelectionChanged: (index, selected) => {
        if (selected && modes[index] !== QuickShareService.visibility)
            QuickShareService.setVisibility(modes[index]);
    }

    Connections {
        target: QuickShareService

        function onVisibilityChanged() {
            picker.sync();
        }
    }
}
