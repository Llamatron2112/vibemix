import QtQuick
import QtQuick.Controls
import QtQuick.Dialogs
import QtQuick.Layouts
import QtQuick.Window
import com.vibemix.gui 1.0

ApplicationWindow {
    id: root
    visible: true
    width: 1180
    height: 760
    minimumWidth: 760
    minimumHeight: 480
    title: root.fileName(backend.session_path) + (backend.dirty ? " *" : "") + " — vibemix"

    property bool forceClose: false
    property var pendingAction: null
    property string sessionError: ""

    Backend {
        id: backend
    }

    readonly property int stripBody: 60
    readonly property int pluginRowHeight: 18
    readonly property int pluginFrame: pluginRowHeight * 2 + 6
    readonly property color accentColor: palette.highlight
    readonly property color panelColor: palette.base
    readonly property color controlColor: palette.button
    readonly property color textColor: palette.windowText
    readonly property color mutedText: Qt.rgba(palette.windowText.r, palette.windowText.g, palette.windowText.b, 0.5)
    readonly property color borderColor: Qt.rgba(palette.windowText.r, palette.windowText.g, palette.windowText.b, 0.25)
    readonly property color meterBack: Qt.rgba(palette.windowText.r, palette.windowText.g, palette.windowText.b, 0.1)

    property var mix: ({ strips: [], buses: [] })
    property var values: ({ strips: {}, buses: {}, params: {} })
    property var levels: ({ strips: {}, buses: {} })
    property var catalog: []
    property var meterSource: ({})
    property var busMeterSource: ({})
    property bool sliderHovered: false
    property bool gripDragging: false

    function val(map, id, key, fallback) {
        var entry = map[id]
        return (entry && entry[key] !== undefined) ? entry[key] : fallback
    }
    function stripGain(id) { return root.val(values.strips, id, "gain", 1) }
    function stripPan(id) { return root.val(values.strips, id, "pan", 0) }
    function stripInputGain(id) { return root.val(values.strips, id, "input_gain_db", 0) }
    function stripMute(id) { return root.val(values.strips, id, "mute", false) }
    function stripSolo(id) { return root.val(values.strips, id, "solo", false) }
    function busGain(id) { return root.val(values.buses, id, "gain", 1) }
    function busMute(id) { return root.val(values.buses, id, "mute", false) }
    function busSendGain(id) { return root.val(values.buses, id, "send_gain", 1) }
    function busReturnGain(id) { return root.val(values.buses, id, "return_gain", 1) }
    function stripIndex(id) {
        for (var i = 0; i < mix.strips.length; i++) if (mix.strips[i].id === id) return i
        return -1
    }
    function busIndex(id) {
        for (var i = 0; i < mix.buses.length; i++) if (mix.buses[i].id === id) return i
        return -1
    }
    function moveStripBy(id, delta) {
        var index = root.stripIndex(id)
        var target = index + delta
        if (index < 0 || target < 0 || target >= mix.strips.length) return
        backend.moveStrip(id, target)
    }
    function moveBusBy(id, delta) {
        var index = root.busIndex(id)
        var target = index + delta
        // Master is pinned last; non-master buses live in [0, len-2].
        if (index < 0 || target < 0 || target > mix.buses.length - 2) return
        backend.moveBus(id, target)
    }
    function dropIndexFor(row, card, dx, kind) {
        var center = card.x + card.width / 2 + dx
        var result = 0
        var kids = row.children
        for (var i = 0; i < kids.length; i++) {
            var child = kids[i]
            if (child === card || child.reorderKind !== kind) continue
            if (center > child.x + child.width / 2) result++
        }
        return result
    }
    function paramValue(location, target, slot, index) {
        var params = values.params[location + "-" + target + "-" + slot]
        return (params && params[index] !== undefined) ? params[index] : 0
    }
    function stripLevel(id, source, channel) {
        var entry = levels.strips[id]
        if (!entry) return 0
        var pair = entry[source]
        return (pair && pair[channel] !== undefined) ? pair[channel] : 0
    }
    function meterSourceFor(id) { return meterSource[id] || "out" }
    function meterSourceLabel(id) {
        var source = root.meterSourceFor(id)
        return source === "gain" ? "GAIN" : source.toUpperCase()
    }
    function cycleMeterSource(id) {
        var order = ["in", "gain", "out"]
        var next = order[(order.indexOf(root.meterSourceFor(id)) + 1) % order.length]
        var copy = Object.assign({}, meterSource)
        copy[id] = next
        meterSource = copy
    }
    function peakDb(id, source) {
        var left = root.stripLevel(id, source, 0)
        var right = root.stripLevel(id, source, 1)
        return 20 * Math.log10(Math.max(left, right, 1.0e-6))
    }
    function busLevel(id, source, channel) {
        var entry = levels.buses[id]
        if (!entry) return 0
        var pair = entry[source]
        return (pair && pair[channel] !== undefined) ? pair[channel] : 0
    }
    function busMeterSourceFor(id) { return busMeterSource[id] || "out" }
    function busMeterSourceLabel(id) {
        return root.busMeterSourceFor(id).toUpperCase()
    }
    function cycleBusMeterSource(id) {
        var order = ["in", "send", "ret", "out"]
        var next = order[(order.indexOf(root.busMeterSourceFor(id)) + 1) % order.length]
        var copy = Object.assign({}, busMeterSource)
        copy[id] = next
        busMeterSource = copy
    }
    function busPeakDb(id, source) {
        var left = root.busLevel(id, source, 0)
        var right = root.busLevel(id, source, 1)
        return 20 * Math.log10(Math.max(left, right, 1.0e-6))
    }
    function formatDb(db) { return Math.max(db, -60).toFixed(1) }
    function gainText(gain) {
        if (gain <= 1.0e-4) return "-inf dB"
        var db = 20 * Math.log10(gain)
        return (db >= 0 ? "+" : "") + db.toFixed(1) + " dB"
    }
    function gainToDb(gain) {
        if (gain <= 1.0e-4) return -60
        return 20 * Math.log10(gain)
    }
    function dbToGain(db) { return Math.pow(10, db / 20) }
    function panFromText(text) {
        var trimmed = String(text).trim()
        if (trimmed.length === 0 || trimmed === "C" || trimmed === "c") return 0
        var upper = trimmed.toUpperCase()
        if (upper[0] === "L" || upper[0] === "R") {
            var magnitude = Number(upper.slice(1).trim())
            if (isNaN(magnitude)) return NaN
            var frac = Math.abs(magnitude) <= 1 ? Math.abs(magnitude) : Math.abs(magnitude) / 100
            return (upper[0] === "L" ? -1 : 1) * Math.min(1, frac)
        }
        var value = Number(trimmed)
        return isNaN(value) ? NaN : value
    }
    function trimText(db) { return (db >= 0 ? "+" : "") + db.toFixed(1) + " dB" }
    function panText(pan) {
        if (Math.abs(pan) < 0.005) return "C"
        return (pan < 0 ? "L " : "R ") + Math.round(Math.abs(pan) * 100)
    }
    function pluginList(location, target) {
        if (location === 2) {
            var buses = mix.buses
            for (var i = 0; i < buses.length; i++) {
                if (buses[i].id === target) return buses[i].plugins || []
            }
            return []
        }
        var strips = mix.strips
        for (var k = 0; k < strips.length; k++) {
            if (strips[k].id !== target) continue
            return location === 1 ? (strips[k].plugins_post || []) : (strips[k].plugins || [])
        }
        return []
    }
    function pluginEntry(location, target, slot) {
        var plugins = root.pluginList(location, target)
        for (var j = 0; j < plugins.length; j++) {
            if (plugins[j].slot === slot) return plugins[j]
        }
        return null
    }
    function pluginName(location, target, slot) {
        var entry = root.pluginEntry(location, target, slot)
        return entry ? entry.name : ""
    }
    function pluginControls(location, target, slot) {
        var entry = root.pluginEntry(location, target, slot)
        return entry ? entry.controls : []
    }
    function pluginHasUi(location, target, slot) {
        var entry = root.pluginEntry(location, target, slot)
        return entry ? entry.has_ui === true : false
    }
    function sendLevel(stripId, busId) {
        var entry = values.strips[stripId]
        if (!entry || !entry.sends) return 0
        var gain = entry.sends[busId]
        return gain === undefined ? 0 : gain
    }
    function sendDelay(stripId, busId) {
        var entry = values.strips[stripId]
        if (!entry || !entry.send_delays) return 0
        var delay = entry.send_delays[busId]
        return delay === undefined ? 0 : delay
    }
    function filteredCatalog(query, channel) {
        var needle = query ? query.toLowerCase() : ""
        return catalog.filter(function(entry) {
            if (channel === "mono" && entry.inputs !== 1) return false
            if (channel === "stereo" && entry.inputs !== 2) return false
            if (!needle) return true
            return entry.name.toLowerCase().indexOf(needle) >= 0
                || entry.uri.toLowerCase().indexOf(needle) >= 0
        })
    }
    function localPath(url) {
        var text = String(url)
        if (text.indexOf("file://") === 0) text = text.slice(7)
        return text
    }

    function transportUi() {
        var t = mix.transport_ui
        return t ? t : ({ beats: true, time: false, loop_indicator: false, loop_bars: false, loop_length: 8 })
    }
    function setTransportUi(patch) {
        var t = root.transportUi()
        var next = {
            beats: t.beats, time: t.time, loop_indicator: t.loop_indicator,
            loop_bars: t.loop_bars, loop_length: t.loop_length
        }
        for (var k in patch) next[k] = patch[k]
        backend.setTransportUi(JSON.stringify(next))
    }
    function clockInfo() { return JSON.parse(backend.clock) }
    function beatsPerBar() {
        var bpb = root.clockInfo().beats_per_bar
        return bpb > 0 ? bpb : 4
    }
    function beatCounter() {
        var c = root.clockInfo()
        var bpb = root.beatsPerBar()
        return (Math.floor(c.beats / bpb) + 1) + "." + (Math.floor(c.beats % bpb) + 1)
    }
    function timeCounter() {
        var s = Math.floor(root.clockInfo().seconds)
        var m = Math.floor(s / 60)
        s = s % 60
        return m + ":" + (s < 10 ? "0" : "") + s
    }
    function loopInfo() {
        var t = root.transportUi()
        var c = root.clockInfo()
        var len = Math.max(1, t.loop_length)
        var total = t.loop_bars ? len * root.beatsPerBar() : len
        var index = Math.floor(((c.beats % total) + total) % total / total * len)
        return { count: len, index: index, bars: t.loop_bars }
    }
    function midiKindName(kind) {
        switch (kind) {
        case 0: return "strip_gain"
        case 1: return "strip_pan"
        case 2: return "strip_mute"
        case 3: return "strip_solo"
        case 4: return "bus_gain"
        case 5: return "bus_mute"
        case 6: return "send_gain"
        case 7: return "send_delay"
        case 8: return "plugin_param"
        case 9: return "plugin_bypass"
        default: return ""
        }
    }
    function bindingSpec(name, paramLabel, kind, target, min, max) {
        return {
            label: name + " — " + paramLabel,
            kind: kind, location: 0, target: target,
            slot: 0, param: 0, min: min, max: max
        }
    }
    function midiBindingRows() {
        var rows = []
        for (var i = 0; i < mix.strips.length; ++i) {
            var s = mix.strips[i]
            var sc = s.midi_channel !== undefined ? s.midi_channel : null
            rows.push({
                label: s.name,
                is_bus: false,
                target: s.id,
                channel: sc,
                cells: [
                    root.bindingSpec(s.name, "fader", 0, s.id, 0.0, 2.0),
                    root.bindingSpec(s.name, "pan", 1, s.id, -1.0, 1.0),
                    root.bindingSpec(s.name, "mute", 2, s.id, 0.0, 1.0),
                    root.bindingSpec(s.name, "solo", 3, s.id, 0.0, 1.0)
                ]
            })
        }
        for (var j = 0; j < mix.buses.length; ++j) {
            var b = mix.buses[j]
            var bc = b.midi_channel !== undefined ? b.midi_channel : null
            rows.push({
                label: b.name,
                is_bus: true,
                target: b.id,
                channel: bc,
                cells: [
                    root.bindingSpec(b.name, "fader", 4, b.id, 0.0, 2.0),
                    null,
                    root.bindingSpec(b.name, "mute", 5, b.id, 0.0, 1.0),
                    null
                ]
            })
        }
        return rows
    }
    function pluginBinding(location, target, slot, param) {
        var list = mix.midi_bindings || []
        for (var i = 0; i < list.length; ++i) {
            var b = list[i]
            if (b.target === "plugin_param" && b.location === location
                    && b.target_id === target && b.slot === slot && b.param === param) {
                return b
            }
        }
        return null
    }
    function paramBindingText(location, target, slot, param) {
        var b = root.pluginBinding(location, target, slot, param)
        if (!b) {
            return ""
        }
        return (b.channel >= 16 ? "any" : String(b.channel + 1)) + ":" + b.cc
    }
    function pluginSlot(location, target, slot) {
        var strips = mix.strips || []
        var buses = mix.buses || []
        var owners = location === 2 ? buses : strips
        for (var i = 0; i < owners.length; ++i) {
            if (owners[i].id !== target) {
                continue
            }
            var plugins = location === 1 ? owners[i].plugins_post : owners[i].plugins
            plugins = plugins || []
            for (var j = 0; j < plugins.length; ++j) {
                if (plugins[j].slot === slot) {
                    return plugins[j]
                }
            }
        }
        return null
    }
    function pluginMidiChannel(location, target, slot) {
        var plugin = root.pluginSlot(location, target, slot)
        return plugin && plugin.midi_channel !== undefined ? plugin.midi_channel : null
    }
    function channelLabel(channel) {
        return channel === null || channel === undefined ? "any" : ("ch " + (channel + 1))
    }
    function midiCcFor(kind, location, target, slot, param) {
        var name = root.midiKindName(kind)
        var list = mix.midi_bindings || []
        for (var i = 0; i < list.length; ++i) {
            var b = list[i]
            if (b.target === name && b.location === location && b.target_id === target
                    && b.slot === slot && b.param === param) {
                return String(b.cc)
            }
        }
        return ""
    }
    function midiBindFromField(kind, location, target, slot, param, min, max, text) {
        var t = String(text).trim()
        if (t === "") {
            backend.midiUnbind(kind, location, target, slot, param)
            return
        }
        var cc = parseInt(t, 10)
        if (isNaN(cc) || cc < 0 || cc > 127) {
            return
        }
        backend.midiBind(kind, location, target, slot, param, min, max, 16, cc)
    }
    function fileName(path) {
        if (!path) return "untitled"
        var parts = String(path).split("/")
        return parts[parts.length - 1] || path
    }
    function recentItems() {
        try { return JSON.parse(backend.recent_files) } catch (error) { return [] }
    }
    function sessionFolder() {
        return backend.session_dir.length > 0 ? "file://" + backend.session_dir : "file:///"
    }
    function guardUnsaved(action) {
        if (!backend.dirty) {
            action()
            return
        }
        pendingAction = action
        dirtyDialog.open()
    }
    function runPending() {
        var action = pendingAction
        pendingAction = null
        if (action) action()
    }
    function saveSessionTo(path) {
        backend.saveSession(path, root.width, root.height,
            root.visibility === Window.Maximized)
    }
    function saveAction(action) {
        if (backend.session_path.length === 0) {
            pendingAction = action
            saveDialog.open()
        } else {
            root.saveSessionTo(backend.session_path)
            if (action) action()
        }
    }
    function requestNew() { root.guardUnsaved(function() { backend.newSession() }) }
    function requestOpen() { root.guardUnsaved(function() { openDialog.open() }) }
    function requestQuit() {
        root.guardUnsaved(function() {
            root.forceClose = true
            root.close()
        })
    }

    onClosing: (close) => {
        if (root.forceClose || !backend.dirty) return
        close.accepted = false
        root.guardUnsaved(function() {
            root.forceClose = true
            root.close()
        })
    }

    menuBar: MenuBar {
        id: appMenuBar

        Menu {
            title: "File"
            MenuItem {
                text: "New"
                enabled: backend.connected
                onTriggered: root.requestNew()
            }
            MenuItem {
                text: "Open…"
                enabled: backend.connected
                onTriggered: root.requestOpen()
            }
            MenuItem {
                text: "Save"
                enabled: backend.connected
                onTriggered: root.saveAction(null)
            }
            MenuItem {
                text: "Save As…"
                enabled: backend.connected
                onTriggered: saveDialog.open()
            }
            MenuSeparator { }
            Menu {
                title: "Recent"
                enabled: root.recentItems().length > 0
                Repeater {
                    model: root.recentItems()
                    MenuItem {
                        text: root.fileName(modelData)
                        onTriggered: root.guardUnsaved(function() {
                            backend.loadSession(modelData)
                        })
                    }
                }
            }
            MenuSeparator { }
            MenuItem {
                text: "Quit"
                onTriggered: root.requestQuit()
            }
        }

        Menu {
            title: "Edit"
            MenuItem {
                text: backend.undo_label.length > 0 ? "Undo " + backend.undo_label : "Undo"
                enabled: backend.connected && backend.undo_depth > 0
                onTriggered: backend.undo()
            }
            MenuItem {
                text: backend.redo_label.length > 0 ? "Redo " + backend.redo_label : "Redo"
                enabled: backend.connected && backend.redo_depth > 0
                onTriggered: backend.redo()
            }
        }

        Menu {
            title: "Add"
            MenuItem {
                text: "Add strip"
                enabled: backend.connected
                onTriggered: backend.addStrip()
            }
            MenuItem {
                text: "Add bus"
                enabled: backend.connected
                onTriggered: backend.addBus()
            }
        }

        Menu {
            title: "MIDI"
            MenuItem {
                text: "MIDI sync"
                checkable: true
                checked: mix.midi_sync === true
                enabled: backend.connected
                onTriggered: backend.setMidiSync(!(mix.midi_sync === true))
            }
            MenuSeparator { }
            MenuItem {
                text: "MIDI bindings…"
                enabled: backend.connected
                onTriggered: midiBindingsWindow.show()
            }
        }

        background: Rectangle {
            color: palette.button

            MouseArea {
                anchors.fill: parent
                acceptedButtons: Qt.RightButton
                onClicked: transportMenu.popup()

                RowLayout {
                    anchors.right: parent.right
                    anchors.rightMargin: 8
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 8

                    Label {
                        visible: root.transportUi().beats
                        text: root.beatCounter()
                        color: backend.clock.indexOf("\"locked\":true") >= 0 ? root.textColor : root.mutedText
                        font.pixelSize: 11
                        font.family: "monospace"
                    }

                    Label {
                        visible: root.transportUi().time
                        text: root.timeCounter()
                        color: root.mutedText
                        font.pixelSize: 11
                        font.family: "monospace"
                    }

                    Label {
                        visible: backend.clock.length > 0 && root.clockInfo().playing
                        text: "\u25B6"
                        color: root.accentColor
                        font.pixelSize: 10
                    }

                    Row {
                        visible: root.transportUi().loop_indicator && backend.clock.length > 0
                        spacing: 2

                        Repeater {
                            model: root.loopInfo().count
                            Rectangle {
                                width: 6
                                height: 6
                                color: index <= root.loopInfo().index ? root.accentColor : root.borderColor
                            }
                        }
                    }

                    Label {
                        text: backend.action
                        visible: backend.action.length > 0
                        color: root.mutedText
                        font.pixelSize: 11
                    }

                    Label {
                        text: backend.xruns + " xruns"
                        visible: backend.xruns > 0
                        color: "#e53935"
                        font.pixelSize: 11
                    }

                    Rectangle {
                        implicitWidth: 8
                        implicitHeight: 8
                        color: backend.connected ? "#43a047" : "#e53935"

                        HoverHandler { id: statusDotHover }
                        ToolTip.visible: statusDotHover.hovered
                        ToolTip.text: backend.connected ? "connected" : "not running"
                        ToolTip.delay: 300
                    }
                }

                Menu {
                    id: transportMenu
                    MenuItem {
                        text: "Beats:bars counter"
                        checkable: true
                        checked: root.transportUi().beats
                        onTriggered: root.setTransportUi({ beats: !root.transportUi().beats })
                    }
                    MenuItem {
                        text: "Minutes:seconds counter"
                        checkable: true
                        checked: root.transportUi().time
                        onTriggered: root.setTransportUi({ time: !root.transportUi().time })
                    }
                    MenuItem {
                        text: "Loop indicator"
                        checkable: true
                        checked: root.transportUi().loop_indicator
                        onTriggered: root.setTransportUi({ loop_indicator: !root.transportUi().loop_indicator })
                    }
                    MenuSeparator { }
                    MenuItem {
                        text: "Loop settings…"
                        onTriggered: loopDialog.open()
                    }
                }
            }
        }
    }

    Shortcut {
        sequence: "Ctrl+S"
        enabled: backend.connected
        onActivated: root.saveAction(null)
    }
    Shortcut {
        sequence: "Ctrl+Shift+S"
        enabled: backend.connected
        onActivated: saveDialog.open()
    }
    Shortcut {
        sequence: "Ctrl+Z"
        enabled: backend.connected && backend.undo_depth > 0
        onActivated: backend.undo()
    }
    Shortcut {
        sequence: "Ctrl+Shift+Z"
        enabled: backend.connected && backend.redo_depth > 0
        onActivated: backend.redo()
    }

    FileDialog {
        id: openDialog
        title: "Open session"
        fileMode: FileDialog.OpenFile
        nameFilters: ["vibemix session (*.toml)", "All files (*)"]
        currentFolder: root.sessionFolder()
        onAccepted: backend.loadSession(root.localPath(selectedFile))
    }

    FileDialog {
        id: saveDialog
        title: "Save session"
        fileMode: FileDialog.SaveFile
        defaultSuffix: "toml"
        nameFilters: ["vibemix session (*.toml)", "All files (*)"]
        currentFolder: root.sessionFolder()
        onAccepted: {
            root.saveSessionTo(root.localPath(selectedFile))
            root.runPending()
        }
    }

    Dialog {
        id: dirtyDialog
        title: "Unsaved changes"
        modal: true
        anchors.centerIn: parent
        standardButtons: Dialog.Save | Dialog.Discard | Dialog.Cancel
        Label {
            text: "This session has unsaved changes. Save before continuing?"
            color: root.textColor
            font.pixelSize: 11
        }
        onAccepted: {
            var after = root.pendingAction
            root.pendingAction = null
            root.saveAction(after)
        }
        onDiscarded: root.runPending()
        onRejected: root.pendingAction = null
    }

    Dialog {
        id: errorDialog
        title: "Session error"
        modal: true
        anchors.centerIn: parent
        standardButtons: Dialog.Ok
        Label {
            text: root.sessionError
            color: root.textColor
            font.pixelSize: 11
        }
    }

    Connections {
        target: backend
        function onSession_resultChanged() {
            var result
            try { result = JSON.parse(backend.session_result) } catch (error) { return }
            if (result && result.ok === false && result.error.length > 0) {
                root.sessionError = result.error
                errorDialog.open()
            }
        }
    }

    Connections {
        target: backend
        function onWindow_geometryChanged() {
            var geometry
            try { geometry = JSON.parse(backend.window_geometry) } catch (error) { return }
            if (!geometry || geometry.width <= 0 || geometry.height <= 0) return
            root.width = Math.max(root.minimumWidth, geometry.width)
            root.height = Math.max(root.minimumHeight, geometry.height)
            root.visibility = geometry.maximized ? Window.Maximized : Window.Windowed
        }
    }
    property var editorWindows: ({})

    function isEnumControl(control) {
        return control !== null && control.scale_points !== undefined
            && control.scale_points !== null && control.scale_points.length > 0
    }
    function enumIndex(control, value) {
        var points = control.scale_points
        var best = 0
        for (var i = 1; i < points.length; i++) {
            if (Math.abs(points[i].value - value) < Math.abs(points[best].value - value)) best = i
        }
        return best
    }
    function controlText(control, value) {
        if (control.toggled === true) return value >= 0.5 ? "on" : "off"
        if (root.isEnumControl(control)) {
            return control.scale_points[root.enumIndex(control, value)].label
        }
        if (control.integer === true) return String(Math.round(value))
        return Number(value).toFixed(3)
    }
    function editText(control, value) {
        if (control.integer === true) return String(Math.round(value))
        var text = Number(value).toFixed(4)
        if (text.indexOf(".") >= 0) text = text.replace(/0+$/, "").replace(/\.$/, "")
        return text
    }
    function commitValue(control, text, location, target, slot, index) {
        var parsed = Number(text)
        if (text === "" || isNaN(parsed)) return
        if (control.integer === true) parsed = Math.round(parsed)
        parsed = Math.max(control.min, Math.min(control.max, parsed))
        backend.setPluginParam(location, target, slot, index, parsed)
    }
    function openEditor(location, target, slot) {
        var key = root.editorKey(location, target, slot)
        var existing = editorWindows[key]
        if (existing) {
            existing.requestActivate()
            return
        }
        var entry = root.pluginEntry(location, target, slot)
        var window = editorComponent.createObject(root, {
            "location": location,
            "target": target,
            "slot": slot,
            "editorKey": key,
            "pluginUri": entry ? entry.uri : ""
        })
        if (window) editorWindows[key] = window
    }
    function closeEditor(key) {
        var window = editorWindows[key]
        if (window) {
            delete editorWindows[key]
            window.destroy()
        }
    }
    function pruneEditors() {
        var keys = Object.keys(editorWindows)
        for (var i = 0; i < keys.length; i++) {
            var window = editorWindows[keys[i]]
            if (!window) continue
            var entry = root.pluginEntry(window.location, window.target, window.slot)
            if (!entry || entry.uri !== window.pluginUri) root.closeEditor(keys[i])
        }
    }
    function editorKey(location, target, slot) {
        return location + ":" + target + ":" + slot
    }
    function openPluginGui(location, target, slot) {
        if (root.pluginHasUi(location, target, slot)) {
            backend.openPluginUi(location, target, slot)
        } else {
            root.openEditor(location, target, slot)
        }
    }
    function openPicker(location, target) {
        pickerWindow.location = location
        pickerWindow.target = target
        pickerFilter.text = ""
        pickerWindow.channelFilter = ""
        pickerWindow.show()
    }

    Connections {
        target: backend
        function onSnapshotChanged() { mix = JSON.parse(backend.snapshot); root.pruneEditors() }
    }
    Connections {
        target: backend
        function onValuesChanged() { values = JSON.parse(backend.values) }
    }
    Connections {
        target: backend
        function onMetersChanged() { levels = JSON.parse(backend.meters) }
    }
    Connections {
        target: backend
        function onCatalogChanged() {
            catalog = JSON.parse(backend.catalog)
            pickerWindow.scanning = false
            scanTimeout.stop()
        }
    }
    Connections {
        target: backend
        function onReportChanged() {
            if (backend.report.length === 0) return
            try { missingDialog.items = JSON.parse(backend.report) }
            catch (error) { missingDialog.items = [] }
            missingDialog.open()
        }
    }

    Timer { interval: 33; running: true; repeat: true; onTriggered: backend.poll() }
    Timer { interval: 1000; running: true; repeat: true; triggeredOnStart: true; onTriggered: backend.refresh() }

    component FlatButton: Button {
        id: button
        property color tint: root.controlColor
        implicitHeight: 22
        leftPadding: 8
        rightPadding: 8
        topPadding: 1
        bottomPadding: 1

        background: Rectangle {
            radius: 0
            color: button.pressed
                ? Qt.darker(button.tint, 1.25)
                : (button.hovered ? Qt.lighter(button.tint, 1.08) : button.tint)
            border.color: root.borderColor
            border.width: 1
        }
        contentItem: Label {
            text: button.text
            color: root.textColor
            font.pixelSize: 11
            horizontalAlignment: Text.AlignHCenter
            verticalAlignment: Text.AlignVCenter
        }
    }

    component BindingCell: Rectangle {
        id: cell
        property var spec: null
        property var bindingHost: null
        implicitWidth: 56
        implicitHeight: 22
        color: (cellHover.hovered && cell.spec !== null) ? root.controlColor : "transparent"
        border.color: root.borderColor
        border.width: 1

        Label {
            anchors.centerIn: parent
            text: cell.spec === null
                ? ""
                : (root.midiCcFor(cell.spec.kind, cell.spec.location, cell.spec.target,
                    cell.spec.slot, cell.spec.param) || "—")
            color: root.textColor
            font.pixelSize: 10
        }

        HoverHandler { id: cellHover }

        MouseArea {
            anchors.fill: parent
            enabled: cell.spec !== null
            onClicked: cellMenu.popup()
        }

        Menu {
            id: cellMenu
            MenuItem {
                text: "Edit…"
                enabled: cell.spec !== null
                onTriggered: {
                    if (cell.bindingHost) {
                        cell.bindingHost.beginEditBinding(cell.spec)
                    }
                }
            }
            MenuItem {
                text: "Learn"
                enabled: cell.spec !== null
                onTriggered: backend.midiLearn(cell.spec.kind, cell.spec.location,
                    cell.spec.target, cell.spec.slot, cell.spec.param,
                    cell.spec.min, cell.spec.max)
            }
            MenuItem {
                text: "Clear"
                enabled: cell.spec !== null
                onTriggered: backend.midiUnbind(cell.spec.kind, cell.spec.location,
                    cell.spec.target, cell.spec.slot, cell.spec.param)
            }
        }
    }

    component NameEdit: Item {
        id: nameEdit
        property alias text: display.text
        signal committed(string value)

        implicitWidth: display.implicitWidth
        implicitHeight: display.implicitHeight

        Label {
            id: display
            anchors.fill: parent
            visible: !editor.visible
            color: root.textColor
            font.bold: true
            font.pixelSize: 11
            elide: Text.ElideRight
            verticalAlignment: Text.AlignVCenter
        }

        MouseArea {
            anchors.fill: parent
            acceptedButtons: Qt.LeftButton
            onDoubleClicked: nameEdit.begin()
        }

        TextField {
            id: editor
            anchors.fill: parent
            visible: false
            color: root.textColor
            font.bold: true
            font.pixelSize: 11
            leftPadding: 0
            rightPadding: 0
            topPadding: 0
            bottomPadding: 0
            selectByMouse: true
            verticalAlignment: TextInput.AlignVCenter
            background: Rectangle { color: root.panelColor }
            onEditingFinished: {
                if (visible) nameEdit.finish(true)
            }
            Keys.onEscapePressed: nameEdit.finish(false)
        }

        function begin() {
            editor.text = display.text
            editor.visible = true
            editor.forceActiveFocus()
            editor.selectAll()
        }
        function finish(commit) {
            editor.visible = false
            if (commit) nameEdit.committed(editor.text)
        }
    }

    component ReorderGrip: Rectangle {
        id: grip
        property var card
        property var row
        property string kind: ""
        property bool reorderEnabled: true
        signal dropped(int index)
        signal menuRequested()

        implicitHeight: 10
        radius: 2
        color: gripHover.hovered || gripDrag.active ? root.accentColor : root.borderColor
        z: 1

        function currentIndex() {
            var kids = grip.row.children
            var index = 0
            for (var i = 0; i < kids.length; i++) {
                if (kids[i].reorderKind !== grip.kind) continue
                if (kids[i] === grip.card) return index
                index++
            }
            return -1
        }

        HoverHandler {
            id: gripHover
            cursorShape: Qt.SizeHorCursor
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onSingleTapped: grip.menuRequested()
        }

        DragHandler {
            id: gripDrag
            target: null
            enabled: grip.reorderEnabled
            yAxis.enabled: false
            cursorShape: Qt.ClosedHandCursor
            onTranslationChanged: grip.card.dragOffset = gripDrag.translation.x
            onActiveChanged: {
                if (!grip.card) return
                if (gripDrag.active) {
                    grip.card.dragging = true
                    root.gripDragging = true
                } else {
                    var index = root.dropIndexFor(grip.row, grip.card, grip.card.dragOffset, grip.kind)
                    grip.card.dragging = false
                    grip.card.dragOffset = 0
                    root.gripDragging = false
                    if (index !== grip.currentIndex()) grip.dropped(index)
                }
            }
        }
    }

    component NumberEntry: Dialog {
        id: entry
        property real from: 0
        property real to: 1
        property string label: ""
        property var format: function(v) { return Number(v).toFixed(3) }
        property var parse: function(t) { return Number(t) }
        signal committed(real value)

        parent: Overlay.overlay
        anchors.centerIn: parent
        modal: true
        title: entry.label.length > 0 ? "Set " + entry.label : "Set value"
        standardButtons: Dialog.Ok | Dialog.Cancel
        implicitWidth: 220

        function openFor(v) {
            entryInput.text = entry.format(v)
            entry.open()
            entryInput.forceActiveFocus()
            entryInput.selectAll()
        }

        onAccepted: {
            var parsed = entry.parse(entryInput.text)
            if (isNaN(parsed)) return
            entry.committed(Math.max(entry.from, Math.min(entry.to, parsed)))
        }

        contentItem: TextField {
            id: entryInput
            implicitHeight: 26
            color: root.textColor
            font.pixelSize: 12
            font.family: "monospace"
            horizontalAlignment: TextInput.AlignRight
            selectByMouse: true
            onAccepted: entry.accept()
            Keys.onEscapePressed: entry.reject()
        }
    }

    component ToggleChip: Button {
        id: chip
        property color tint: root.accentColor
        implicitWidth: 24
        implicitHeight: 22
        checkable: true
        padding: 0

        background: Rectangle {
            radius: 0
            color: chip.checked ? chip.tint : root.controlColor
            border.color: chip.checked ? Qt.lighter(chip.tint, 1.3) : root.borderColor
            border.width: 1
        }
        contentItem: Label {
            text: chip.text
            color: chip.checked ? "#ffffff" : root.textColor
            horizontalAlignment: Text.AlignHCenter
            verticalAlignment: Text.AlignVCenter
            font.pixelSize: 11
            font.bold: true
        }
    }

    component PluginList: Column {
        id: pluginListRoot
        property int location: 0
        property int target: -1
        property var plugins: []
        property string title: ""
        spacing: 2

        Label {
            text: pluginListRoot.title
            color: root.mutedText
            font.pixelSize: 8
        }

        Rectangle {
            width: root.stripBody
            height: root.pluginFrame
            color: Qt.rgba(root.textColor.r, root.textColor.g, root.textColor.b, 0.04)
            border.color: root.borderColor
            border.width: 1
            clip: true

            // Right-click the empty space (empty chain or below the rows) for
            // chain-level actions. The ListView is parented to this MouseArea
            // so right-clicks on blank areas bubble up to it; row delegates
            // keep their own menu on top.
            MouseArea {
                id: slotBackground
                anchors.fill: parent
                acceptedButtons: Qt.RightButton
                hoverEnabled: true
                onClicked: slotMenu.popup()

                Menu {
                    id: slotMenu
                    MenuItem {
                        text: "Add plugin…"
                        onTriggered: root.openPicker(pluginListRoot.location, pluginListRoot.target)
                    }
                    MenuItem {
                        text: backend.clipboard_name.length > 0
                            ? "Paste \"" + backend.clipboard_name + "\""
                            : "Paste"
                        enabled: backend.has_clipboard
                        onTriggered: backend.pastePlugin(
                            pluginListRoot.location,
                            pluginListRoot.target)
                    }
                }
            }

            ListView {
                id: listView
                parent: slotBackground
                anchors.fill: parent
                anchors.margins: 1
                clip: true
                spacing: 1
                model: pluginListRoot.plugins
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: ScrollBar {
                    width: 6
                    policy: ScrollBar.AsNeeded
                }

                delegate: Rectangle {
                    id: pluginRow
                    property var pluginData: modelData
                    property int pluginIndex: index
                    property int pluginCount: pluginListRoot.plugins.length
                    width: ListView.view.width
                    height: root.pluginRowHeight
                    color: pluginMouse.containsMouse
                        ? Qt.rgba(root.textColor.r, root.textColor.g, root.textColor.b, 0.18)
                        : "transparent"

                    ToolTip.visible: pluginMouse.containsMouse
                    ToolTip.text: pluginRow.pluginData.name
                    ToolTip.delay: 350

                    Label {
                        anchors.fill: parent
                        anchors.leftMargin: 3
                        anchors.rightMargin: 8
                        text: pluginRow.pluginData.name
                        elide: Text.ElideRight
                        verticalAlignment: Text.AlignVCenter
                        color: pluginRow.pluginData.missing
                            ? "#e53935"
                            : (pluginRow.pluginData.bypassed ? root.mutedText : root.textColor)
                        font.pixelSize: 10
                    }

                    MouseArea {
                        id: pluginMouse
                        anchors.fill: parent
                        hoverEnabled: true
                        acceptedButtons: Qt.LeftButton | Qt.RightButton
                        onClicked: (mouse) => {
                            if (mouse.button === Qt.RightButton) {
                                pluginMenu.popup()
                            } else {
                                root.openPluginGui(pluginListRoot.location, pluginListRoot.target, pluginRow.pluginData.slot)
                            }
                        }
                    }

                    Menu {
                        id: pluginMenu
                        MenuItem {
                            text: "Open GUI"
                            enabled: pluginRow.pluginData.missing !== true
                            onTriggered: root.openPluginGui(pluginListRoot.location, pluginListRoot.target, pluginRow.pluginData.slot)
                        }
                        MenuItem {
                            text: "Parameters..."
                            enabled: pluginRow.pluginData.missing !== true
                            onTriggered: root.openEditor(pluginListRoot.location, pluginListRoot.target, pluginRow.pluginData.slot)
                        }
                        MenuItem {
                            text: "Bypass"
                            checkable: true
                            checked: pluginRow.pluginData.bypassed
                            onTriggered: backend.setPluginBypass(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginData.slot,
                                !pluginRow.pluginData.bypassed)
                        }
                        MenuSeparator { }
                        MenuItem {
                            text: "Move up"
                            enabled: pluginRow.pluginIndex > 0
                            onTriggered: backend.movePlugin(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginIndex,
                                pluginRow.pluginIndex - 1)
                        }
                        MenuItem {
                            text: "Move down"
                            enabled: pluginRow.pluginIndex < pluginRow.pluginCount - 1
                            onTriggered: backend.movePlugin(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginIndex,
                                pluginRow.pluginIndex + 1)
                        }
                        MenuSeparator { }
                        MenuItem {
                            text: "Copy"
                            enabled: pluginRow.pluginData.missing !== true
                            onTriggered: backend.copyPlugin(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginData.slot)
                        }
                        MenuItem {
                            text: "Cut"
                            enabled: pluginRow.pluginData.missing !== true
                            onTriggered: backend.cutPlugin(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginData.slot)
                        }
                        MenuItem {
                            text: backend.clipboard_name.length > 0
                                ? "Paste \"" + backend.clipboard_name + "\""
                                : "Paste"
                            enabled: backend.has_clipboard
                            onTriggered: backend.pastePlugin(
                                pluginListRoot.location,
                                pluginListRoot.target)
                        }
                        MenuSeparator { }
                        MenuItem {
                            text: "Remove plugin"
                            onTriggered: backend.removePlugin(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginData.slot)
                        }
                        MenuSeparator { }
                        MenuItem {
                            text: "Add plugin…"
                            onTriggered: root.openPicker(pluginListRoot.location, pluginListRoot.target)
                        }
                    }
                }
            }
        }
    }

    component Fader: Item {
        id: fader
        property real from: 0
        property real to: 2
        property real value: 1
        property real resetValue: 1
        property real dragStart: 0
        property string valueText: ""
        property string entryUnit: "db"
        readonly property bool interacting: dragHandler.active || valueTimer.running
        signal moved(real value)

        implicitWidth: 22
        implicitHeight: 150

        function bump() { valueTimer.restart() }
        function entryFrom() { return entryUnit === "db" ? -60 : from }
        function entryTo() { return entryUnit === "db" ? root.gainToDb(Math.max(to, 1.0e-6)) : to }
        function entryFormat(v) { return entryUnit === "db" ? root.gainToDb(v).toFixed(1) : Number(v).toFixed(3) }
        function entryParse(t) {
            if (entryUnit === "db") {
                var db = Number(t)
                return isNaN(db) ? NaN : root.dbToGain(db)
            }
            return Number(t)
        }

        Timer { id: valueTimer; interval: 700 }

        readonly property real normalized: (to > from) ? Math.max(0, Math.min(1, (value - from) / (to - from))) : 0

        Rectangle {
            id: track
            width: 5
            height: parent.height - 14
            anchors.centerIn: parent
            color: root.meterBack
            border.color: root.borderColor
            border.width: 1
        }

        Rectangle {
            width: track.width
            anchors.horizontalCenter: track.horizontalCenter
            anchors.bottom: track.bottom
            height: fader.normalized * (track.height - 2)
            color: root.accentColor
        }

        Rectangle {
            id: thumb
            width: parent.width
            height: 10
            anchors.horizontalCenter: parent.horizontalCenter
            y: track.y + (1 - fader.normalized) * (track.height - thumb.height)
            color: dragHandler.active ? Qt.lighter(root.controlColor, 1.2) : root.controlColor
            border.color: dragHandler.active ? root.accentColor : root.borderColor
            border.width: 1
        }

        DragHandler {
            id: dragHandler
            target: null
            xAxis.enabled: false
            grabPermissions: PointerHandler.TakeOverForbidden
            onActiveChanged: {
                if (dragHandler.active) {
                    fader.dragStart = fader.value
                    fader.bump()
                }
            }
            onTranslationChanged: {
                var span = fader.to - fader.from
                var delta = -dragHandler.translation.y / track.height * span
                fader.moved(Math.max(fader.from, Math.min(fader.to, fader.dragStart + delta)))
                fader.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.NoModifier
            onSingleTapped: (eventPoint) => {
                var y = eventPoint.position.y
                var clamped = Math.max(track.y, Math.min(track.y + track.height, y))
                var n = 1 - (clamped - track.y) / track.height
                fader.moved(fader.from + n * (fader.to - fader.from))
                fader.bump()
            }
            onDoubleTapped: {
                fader.moved(fader.resetValue)
                fader.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                fader.moved(fader.resetValue)
                fader.bump()
            }
        }

        WheelHandler {
            onWheel: (e) => {
                var step = (fader.to - fader.from) * 0.03 * (e.angleDelta.y > 0 ? 1 : -1)
                fader.moved(Math.max(fader.from, Math.min(fader.to, fader.value + step)))
                fader.bump()
            }
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onSingleTapped: faderMenu.popup()
        }

        Menu {
            id: faderMenu
            MenuItem {
                text: "Set value…"
                onTriggered: faderEntry.openFor(fader.value)
            }
        }

        NumberEntry {
            id: faderEntry
            label: "gain"
            from: fader.entryFrom()
            to: fader.entryTo()
            format: fader.entryFormat
            parse: fader.entryParse
            onCommitted: (v) => {
                fader.moved(v)
                fader.bump()
            }
        }

        Rectangle {
            z: 100
            visible: fader.interacting && fader.valueText.length > 0
            anchors.horizontalCenter: parent.horizontalCenter
            y: -height - 2
            width: valueLabel.implicitWidth + 8
            height: valueLabel.implicitHeight + 2
            color: root.panelColor
            border.color: root.accentColor
            border.width: 1

            Label {
                id: valueLabel
                anchors.centerIn: parent
                text: fader.valueText
                color: root.textColor
                font.pixelSize: 9
                font.family: "monospace"
            }
        }
    }

    component SendSlider: Item {
        id: slider
        property real value: 0
        property real resetValue: 0
        property bool dragging: false
        property real liveValue: 0
        property real delay: 0
        property var formatValue: function(v) { return Number(v).toFixed(2) }
        readonly property string valueText: slider.formatValue ? slider.formatValue(slider.displayValue) : ""
        readonly property real displayValue: slider.dragging ? slider.liveValue : slider.value
        readonly property bool interacting: slider.dragging || valueTimer.running
        signal moved(real value)
        signal delayMoved(real ms)

        implicitWidth: 56
        implicitHeight: 10

        function bump() { valueTimer.restart() }
        function norm(v) { return Math.max(0, Math.min(1, v)) }
        function setFromX(px) {
            slider.liveValue = slider.norm(px / track.width)
            slider.moved(slider.liveValue)
        }
        function entryFrom() { return -60 }
        function entryTo() { return root.gainToDb(1.0) }
        function entryFormat(v) { return root.gainToDb(v).toFixed(1) }
        function entryParse(t) {
            var db = Number(t)
            return isNaN(db) ? NaN : root.dbToGain(db)
        }
        function sampleRate() {
            return backend.sample_rate > 0 ? backend.sample_rate : 48000
        }
        function delayFormat(v) {
            return Number(v).toFixed(3) + " ms"
        }
        function delayParse(text) {
            var t = String(text).trim().toLowerCase()
            if (t.length === 0) return NaN
            var value
            if (t.indexOf("ms") >= 0) {
                value = Number(t.replace("ms", "").trim())
            } else if (t.indexOf("samples") >= 0) {
                value = Number(t.replace("samples", "").trim())
                return isNaN(value) ? NaN : value * 1000.0 / sampleRate()
            } else if (t.indexOf("sample") >= 0) {
                value = Number(t.replace("sample", "").trim())
                return isNaN(value) ? NaN : value * 1000.0 / sampleRate()
            } else if (t.indexOf("smp") >= 0) {
                value = Number(t.replace("smp", "").trim())
                return isNaN(value) ? NaN : value * 1000.0 / sampleRate()
            } else if (t.charAt(t.length - 1) === "s") {
                value = Number(t.slice(0, -1).trim())
                return isNaN(value) ? NaN : value * 1000.0
            } else {
                value = Number(t)
            }
            return isNaN(value) ? NaN : value
        }

        Timer { id: valueTimer; interval: 700 }

        readonly property real normalized: slider.norm(slider.displayValue)

        Rectangle {
            id: track
            anchors.fill: parent
            color: root.meterBack
            border.color: root.borderColor
            border.width: 1
        }

        Rectangle {
            height: track.height - 2
            anchors.verticalCenter: track.verticalCenter
            x: 1
            width: slider.normalized * (track.width - 2)
            color: root.accentColor
        }

        Rectangle {
            id: thumb
            width: 6
            height: track.height
            x: slider.normalized * (track.width - thumb.width)
            color: slider.dragging ? Qt.lighter(root.controlColor, 1.2) : root.controlColor
            border.color: slider.dragging ? root.accentColor : root.borderColor
            border.width: 1
        }

        MouseArea {
            anchors.fill: parent
            acceptedButtons: Qt.LeftButton
            preventStealing: true
            hoverEnabled: true
            function refreshHover() { root.sliderHovered = containsMouse || slider.dragging }
            onContainsMouseChanged: refreshHover()
            onPressed: (mouse) => {
                slider.liveValue = slider.value
                slider.dragging = true
                refreshHover()
                slider.setFromX(mouse.x)
                slider.bump()
            }
            onPositionChanged: (mouse) => {
                if (slider.dragging) {
                    slider.setFromX(mouse.x)
                    slider.bump()
                }
            }
            onReleased: {
                slider.dragging = false
                refreshHover()
            }
            onCanceled: {
                slider.dragging = false
                refreshHover()
            }
            onDoubleClicked: {
                slider.moved(slider.resetValue)
                slider.bump()
            }
            onWheel: (wheel) => {
                var step = wheel.angleDelta.y > 0 ? 0.03 : -0.03
                slider.moved(slider.norm(slider.value + step))
                slider.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                slider.moved(slider.resetValue)
                slider.bump()
            }
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onSingleTapped: sendMenu.popup()
        }

        Menu {
            id: sendMenu
            MenuItem {
                text: "Set value…"
                onTriggered: sendEntry.openFor(slider.value)
            }
            MenuItem {
                text: "Set delay…"
                onTriggered: sendDelayEntry.openFor(slider.delay)
            }
        }

        NumberEntry {
            id: sendEntry
            label: "send"
            from: slider.entryFrom()
            to: slider.entryTo()
            format: slider.entryFormat
            parse: slider.entryParse
            onCommitted: (v) => {
                slider.moved(v)
                slider.bump()
            }
        }

        NumberEntry {
            id: sendDelayEntry
            label: "send delay (ms · smp · s)"
            from: 0
            to: 500
            format: slider.delayFormat
            parse: slider.delayParse
            onCommitted: (v) => {
                slider.delayMoved(v)
                slider.bump()
            }
        }

        Rectangle {
            z: 100
            visible: slider.interacting && slider.valueText.length > 0
            anchors.horizontalCenter: parent.horizontalCenter
            y: -height - 2
            width: sendValueLabel.implicitWidth + 8
            height: sendValueLabel.implicitHeight + 2
            color: root.panelColor
            border.color: root.accentColor
            border.width: 1

            Label {
                id: sendValueLabel
                anchors.centerIn: parent
                text: slider.valueText
                color: root.textColor
                font.pixelSize: 9
                font.family: "monospace"
            }
        }
    }

    component StereoMeter: Item {
        id: meter
        property real levelL: 0
        property real levelR: 0
        property real floorDb: -60
        property real topDb: 6
        property real holdL: -1000
        property real holdR: -1000
        property bool showScale: true
        property var scaleMajor: [0, -6, -12, -18, -24, -36, -48, -60]
        property var scaleMinor: [6, 3, -3, -9, -15, -21, -27, -30, -33, -39, -42, -45, -51, -54, -57]
        readonly property var scaleTicks: scaleMajor.concat(scaleMinor)
        readonly property int scaleWidth: showScale ? 24 : 0

        implicitWidth: 14 + scaleWidth
        implicitHeight: 150

        readonly property real dbL: 20 * Math.log10(Math.max(meter.levelL, 1.0e-6))
        readonly property real dbR: 20 * Math.log10(Math.max(meter.levelR, 1.0e-6))

        function norm(db) {
            return Math.max(0, Math.min(1, (db - meter.floorDb) / (meter.topDb - meter.floorDb)))
        }

        Timer {
            interval: 60
            running: true
            repeat: true
            onTriggered: {
                meter.holdL = Math.max(meter.dbL, meter.holdL - 1.5)
                meter.holdR = Math.max(meter.dbR, meter.holdR - 1.5)
            }
        }

        Rectangle {
            x: 0
            width: 5
            height: parent.height
            color: root.meterBack
            border.color: root.borderColor
            border.width: 1
        }
        Rectangle {
            x: 0
            width: 5
            anchors.bottom: parent.bottom
            height: meter.norm(meter.dbL) * parent.height
            clip: true
            Rectangle {
                width: parent.width
                height: meter.height
                anchors.bottom: parent.bottom
                gradient: Gradient {
                    GradientStop { position: 0.0; color: "#e53935" }
                    GradientStop { position: 0.3; color: "#fbc02d" }
                    GradientStop { position: 0.55; color: "#43a047" }
                }
            }
        }
        Rectangle {
            x: 0
            width: 5
            height: 1
            visible: meter.holdL > meter.floorDb
            color: root.textColor
            y: parent.height - meter.norm(meter.holdL) * parent.height
        }

        Rectangle {
            x: 8
            width: 5
            height: parent.height
            color: root.meterBack
            border.color: root.borderColor
            border.width: 1
        }
        Rectangle {
            x: 8
            width: 5
            anchors.bottom: parent.bottom
            height: meter.norm(meter.dbR) * parent.height
            clip: true
            Rectangle {
                width: parent.width
                height: meter.height
                anchors.bottom: parent.bottom
                gradient: Gradient {
                    GradientStop { position: 0.0; color: "#e53935" }
                    GradientStop { position: 0.3; color: "#fbc02d" }
                    GradientStop { position: 0.55; color: "#43a047" }
                }
            }
        }
        Rectangle {
            x: 8
            width: 5
            height: 1
            visible: meter.holdR > meter.floorDb
            color: root.textColor
            y: parent.height - meter.norm(meter.holdR) * parent.height
        }

        Repeater {
            model: meter.showScale ? meter.scaleTicks : []
            delegate: Rectangle {
                readonly property bool major: meter.scaleMajor.indexOf(modelData) >= 0
                x: 14
                width: major ? 4 : 2
                height: 1
                color: major ? root.mutedText : root.borderColor
                y: meter.height * (1 - meter.norm(modelData))
            }
        }

        Repeater {
            model: meter.showScale ? meter.scaleMajor : []
            delegate: Label {
                x: 19
                readonly property real lineY: meter.height * (1 - meter.norm(modelData))
                y: Math.max(4, Math.min(meter.height - 4, lineY)) - implicitHeight / 2
                text: (modelData > 0 ? "+" : "") + modelData
                color: root.mutedText
                font.pixelSize: 7
                font.family: "monospace"
            }
        }
    }

    component Knob: Item {
        id: knob
        property real from: -1
        property real to: 1
        property real value: 0
        property real dragStart: 0
        property real resetValue: (from + to) / 2
        property string valueText: ""
        property string entryUnit: "linear"
        readonly property bool interacting: knobDrag.active || valueTimer.running
        signal moved(real value)

        implicitWidth: 26
        implicitHeight: 26

        function bump() { valueTimer.restart() }
        function entryLabel() {
            if (entryUnit === "pan") return "pan"
            if (entryUnit === "db") return "trim"
            if (entryUnit === "gain-db") return "return"
            return "value"
        }
        function entryFrom() {
            if (entryUnit === "gain-db" || entryUnit === "pan") return entryUnit === "pan" ? -1 : -60
            return from
        }
        function entryTo() {
            if (entryUnit === "gain-db") return root.gainToDb(Math.max(to, 1.0e-6))
            if (entryUnit === "pan") return 1
            return to
        }
        function entryFormat(v) {
            if (entryUnit === "gain-db") return root.gainToDb(v).toFixed(1)
            if (entryUnit === "pan") return root.panText(v)
            if (entryUnit === "db") return (v >= 0 ? "+" : "") + Number(v).toFixed(1)
            return Number(v).toFixed(3)
        }
        function entryParse(t) {
            if (entryUnit === "gain-db") {
                var db = Number(t)
                return isNaN(db) ? NaN : root.dbToGain(db)
            }
            if (entryUnit === "pan") return root.panFromText(t)
            return Number(t)
        }

        Timer { id: valueTimer; interval: 700 }

        readonly property real normalized: (to > from) ? Math.max(0, Math.min(1, (value - from) / (to - from))) : 0

        Rectangle {
            anchors.fill: parent
            radius: width / 2
            color: root.controlColor
            border.color: knobDrag.active ? root.accentColor : root.borderColor
            border.width: 1
        }

        Rectangle {
            width: 2
            height: parent.height * 0.3
            color: root.accentColor
            anchors.horizontalCenter: parent.horizontalCenter
            y: parent.height / 2 - height
            transformOrigin: Item.Bottom
            rotation: -135 + knob.normalized * 270
        }

        DragHandler {
            id: knobDrag
            target: null
            xAxis.enabled: false
            grabPermissions: PointerHandler.TakeOverForbidden
            onActiveChanged: {
                if (knobDrag.active) {
                    knob.dragStart = knob.value
                    knob.bump()
                }
            }
            onTranslationChanged: {
                var delta = -knobDrag.translation.y / 120 * (knob.to - knob.from)
                knob.moved(Math.max(knob.from, Math.min(knob.to, knob.dragStart + delta)))
                knob.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.NoModifier
            onDoubleTapped: {
                knob.moved(knob.resetValue)
                knob.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                knob.moved(knob.resetValue)
                knob.bump()
            }
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onSingleTapped: knobMenu.popup()
        }

        Menu {
            id: knobMenu
            MenuItem {
                text: "Set value…"
                onTriggered: knobEntry.openFor(knob.value)
            }
        }

        NumberEntry {
            id: knobEntry
            label: knob.entryLabel()
            from: knob.entryFrom()
            to: knob.entryTo()
            format: knob.entryFormat
            parse: knob.entryParse
            onCommitted: (v) => {
                knob.moved(v)
                knob.bump()
            }
        }

        Rectangle {
            z: 100
            visible: knob.interacting && knob.valueText.length > 0
            anchors.horizontalCenter: parent.horizontalCenter
            y: -height - 2
            width: knobValueLabel.implicitWidth + 8
            height: knobValueLabel.implicitHeight + 2
            color: root.panelColor
            border.color: root.accentColor
            border.width: 1

            Label {
                id: knobValueLabel
                anchors.centerIn: parent
                text: knob.valueText
                color: root.textColor
                font.pixelSize: 9
                font.family: "monospace"
            }
        }
    }

    component ParameterRow: Row {
        id: parameterRow
        property var controlData
        property int paramIndex
        property int paramLocation
        property int paramTarget
        property int paramSlot
        property var bindingHost: null
        spacing: 6

        readonly property real currentValue: root.paramValue(paramLocation, paramTarget, paramSlot, paramIndex)
        readonly property bool isEnum: root.isEnumControl(controlData)

        Label {
            width: 150
            elide: Text.ElideRight
            text: parameterRow.controlData.name
            color: root.textColor
            font.pixelSize: 11
            anchors.verticalCenter: parent.verticalCenter
        }

        CheckBox {
            visible: parameterRow.controlData.toggled === true
            text: parameterRow.currentValue >= 0.5 ? "on" : "off"
            checked: parameterRow.currentValue >= 0.5
            font.pixelSize: 11
            anchors.verticalCenter: parent.verticalCenter
            onClicked: backend.setPluginParam(
                parameterRow.paramLocation,
                parameterRow.paramTarget,
                parameterRow.paramSlot,
                parameterRow.paramIndex,
                checked ? 1 : 0)
        }

        ComboBox {
            visible: parameterRow.controlData.toggled !== true && parameterRow.isEnum
            width: 200
            implicitHeight: 24
            font.pixelSize: 11
            model: parameterRow.controlData.scale_points
            textRole: "label"
            currentIndex: parameterRow.isEnum
                ? root.enumIndex(parameterRow.controlData, parameterRow.currentValue)
                : -1
            anchors.verticalCenter: parent.verticalCenter
            onActivated: backend.setPluginParam(
                parameterRow.paramLocation,
                parameterRow.paramTarget,
                parameterRow.paramSlot,
                parameterRow.paramIndex,
                parameterRow.controlData.scale_points[currentIndex].value)
        }

        Slider {
            visible: parameterRow.controlData.toggled !== true && !parameterRow.isEnum
            width: 200
            implicitHeight: 24
            from: parameterRow.controlData.min
            to: parameterRow.controlData.max
            stepSize: parameterRow.controlData.integer === true ? 1 : 0
            snapMode: parameterRow.controlData.integer === true ? Slider.SnapAlways : Slider.NoSnap
            value: parameterRow.currentValue
            anchors.verticalCenter: parent.verticalCenter
            onMoved: backend.setPluginParam(
                parameterRow.paramLocation,
                parameterRow.paramTarget,
                parameterRow.paramSlot,
                parameterRow.paramIndex,
                value)
        }

        Item {
            width: 78
            height: 24
            visible: parameterRow.controlData.toggled !== true && !parameterRow.isEnum
            anchors.verticalCenter: parent.verticalCenter

            Label {
                id: valueLabel
                anchors.fill: parent
                visible: !valueField.visible
                text: root.controlText(parameterRow.controlData, parameterRow.currentValue)
                color: root.mutedText
                font.pixelSize: 11
                horizontalAlignment: Text.AlignRight
                verticalAlignment: Text.AlignVCenter
                elide: Text.ElideRight

                TapHandler {
                    onDoubleTapped: {
                        valueField.text = root.editText(parameterRow.controlData, parameterRow.currentValue)
                        valueField.visible = true
                        valueField.forceActiveFocus()
                        valueField.selectAll()
                    }
                }
            }

            TextField {
                id: valueField
                anchors.fill: parent
                visible: false
                horizontalAlignment: TextInput.AlignRight
                font.pixelSize: 11
                selectByMouse: true
                onEditingFinished: {
                    root.commitValue(
                        parameterRow.controlData,
                        valueField.text,
                        parameterRow.paramLocation,
                        parameterRow.paramTarget,
                        parameterRow.paramSlot,
                        parameterRow.paramIndex)
                    valueField.visible = false
                }
                Keys.onEscapePressed: valueField.visible = false
            }
        }

        Label {
            width: 56
            text: root.paramBindingText(parameterRow.paramLocation, parameterRow.paramTarget, parameterRow.paramSlot, parameterRow.paramIndex)
            color: root.mutedText
            font.pixelSize: 10
            horizontalAlignment: Text.AlignRight
            verticalAlignment: Text.AlignVCenter
            anchors.verticalCenter: parent.verticalCenter
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onTapped: parameterMenu.popup()
        }

        Menu {
            id: parameterMenu
            MenuItem {
                text: "MIDI binding…"
                onTriggered: {
                    if (parameterRow.bindingHost) {
                        parameterRow.bindingHost.beginParamBinding(
                            parameterRow.paramLocation,
                            parameterRow.paramTarget,
                            parameterRow.paramSlot,
                            parameterRow.paramIndex,
                            parameterRow.controlData.min,
                            parameterRow.controlData.max,
                            parameterRow.controlData.name)
                    }
                }
            }
            MenuItem {
                text: "clear binding"
                enabled: root.paramBindingText(parameterRow.paramLocation, parameterRow.paramTarget, parameterRow.paramSlot, parameterRow.paramIndex) !== ""
                onTriggered: backend.midiUnbind(
                    parameterRow.paramLocation,
                    parameterRow.paramTarget,
                    parameterRow.paramSlot,
                    parameterRow.paramIndex)
            }
        }
    }

    Component {
        id: editorComponent

        Window {
            id: editorWindow
            property int location: 0
            property int target: -1
            property int slot: -1
            property string editorKey: ""
            property string pluginUri: ""
            property var bindingSpec: null
            visible: true
            width: 500
            height: 560
            minimumWidth: 380
            minimumHeight: 240
            title: root.pluginName(editorWindow.location, editorWindow.target, editorWindow.slot)
            color: root.palette.window

            function beginParamBinding(location, target, slot, param, min, max, label) {
                var lo = (min === undefined || isNaN(min)) ? 0 : min
                var hi = (max === undefined || isNaN(max)) ? 1 : max
                editorWindow.bindingSpec = {
                    label: label, kind: 8, location: location, target: target,
                    slot: slot, param: param, min: lo, max: hi
                }
                var existing = root.pluginBinding(location, target, slot, param)
                paramEditCc.text = existing ? String(existing.cc) : ""
                paramEdit.open()
            }

            function commitParamBinding() {
                var spec = editorWindow.bindingSpec
                if (!spec) {
                    paramEdit.close()
                    return
                }
                var cc = parseInt(paramEditCc.text, 10)
                if (!isNaN(cc) && cc >= 0 && cc <= 127) {
                    backend.midiBind(spec.kind, spec.location, spec.target, spec.slot, spec.param, spec.min, spec.max, 16, cc)
                }
                paramEdit.close()
            }

            function learnParamBinding() {
                var spec = editorWindow.bindingSpec
                if (spec) {
                    backend.midiLearn(spec.kind, spec.location, spec.target, spec.slot, spec.param, spec.min, spec.max)
                }
                paramEdit.close()
            }

            function clearParamBinding() {
                var spec = editorWindow.bindingSpec
                if (spec) {
                    backend.midiUnbind(spec.kind, spec.location, spec.target, spec.slot, spec.param)
                }
                paramEdit.close()
            }

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 8
                spacing: 8

                RowLayout {
                    spacing: 6
                    Label {
                        Layout.fillWidth: true
                        text: root.pluginName(editorWindow.location, editorWindow.target, editorWindow.slot)
                        font.bold: true
                        font.pixelSize: 12
                        elide: Text.ElideRight
                        color: root.textColor
                    }
                    FlatButton {
                        text: "native UI"
                        enabled: root.pluginHasUi(editorWindow.location, editorWindow.target, editorWindow.slot)
                        onClicked: backend.openPluginUi(editorWindow.location, editorWindow.target, editorWindow.slot)
                    }
                    FlatButton {
                        text: "close"
                        onClicked: editorWindow.close()
                    }
                }

                RowLayout {
                    spacing: 6

                    Label {
                        text: "MIDI channel"
                        color: root.mutedText
                        font.pixelSize: 11
                    }

                    FlatButton {
                        text: root.channelLabel(root.pluginMidiChannel(editorWindow.location, editorWindow.target, editorWindow.slot))
                        onClicked: pluginChannelMenu.popup()

                        Menu {
                            id: pluginChannelMenu
                            MenuItem {
                                text: "any"
                                onTriggered: backend.setPluginMidiChannel(editorWindow.location, editorWindow.target, editorWindow.slot, 16)
                            }
                            MenuSeparator {}
                            Repeater {
                                model: 16
                                MenuItem {
                                    text: "ch " + (index + 1)
                                    onTriggered: backend.setPluginMidiChannel(editorWindow.location, editorWindow.target, editorWindow.slot, index)
                                }
                            }
                        }
                    }

                    Label {
                        Layout.fillWidth: true
                        text: "parameters learn only on this channel"
                        color: root.mutedText
                        font.pixelSize: 10
                        elide: Text.ElideRight
                    }
                }

                Label {
                    Layout.fillWidth: true
                    visible: root.pluginControls(editorWindow.location, editorWindow.target, editorWindow.slot).length === 0
                    text: "this plugin has no parameters"
                    color: root.mutedText
                    font.pixelSize: 11
                }

                ScrollView {
                    id: editorScroll
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true

                    Column {
                        width: editorScroll.availableWidth
                        spacing: 6

                        Repeater {
                            model: root.pluginControls(editorWindow.location, editorWindow.target, editorWindow.slot)

                            delegate: ParameterRow {
                                controlData: modelData
                                paramIndex: index
                                paramLocation: editorWindow.location
                                paramTarget: editorWindow.target
                                paramSlot: editorWindow.slot
                                bindingHost: editorWindow
                            }
                        }
                    }
                }
            }

            Dialog {
                id: paramEdit
                title: "MIDI binding"
                modal: true
                anchors.centerIn: parent
                width: 300
                padding: 10

                contentItem: ColumnLayout {
                    width: paramEdit.availableWidth
                    spacing: 8

                    Label {
                        Layout.fillWidth: true
                        text: editorWindow.bindingSpec ? editorWindow.bindingSpec.label : ""
                        color: root.textColor
                        elide: Text.ElideRight
                        font.pixelSize: 11
                    }

                    Label {
                        Layout.fillWidth: true
                        text: "channel: " + root.channelLabel(root.pluginMidiChannel(editorWindow.location, editorWindow.target, editorWindow.slot))
                        color: root.mutedText
                        font.pixelSize: 11
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 6

                        Label {
                            text: "CC"
                            color: root.mutedText
                            font.pixelSize: 11
                        }
                        TextField {
                            id: paramEditCc
                            Layout.preferredWidth: 56
                            implicitHeight: 24
                            placeholderText: "0-127"
                            inputMethodHints: Qt.ImhDigitsOnly
                            onAccepted: editorWindow.commitParamBinding()
                        }
                        Item { Layout.fillWidth: true }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 6

                        FlatButton {
                            Layout.preferredWidth: 64
                            text: "learn"
                            onClicked: editorWindow.learnParamBinding()
                        }
                        FlatButton {
                            Layout.preferredWidth: 64
                            text: "clear"
                            onClicked: editorWindow.clearParamBinding()
                        }
                        Item { Layout.fillWidth: true }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 6

                        Item { Layout.fillWidth: true }
                        FlatButton {
                            Layout.preferredWidth: 72
                            text: "cancel"
                            onClicked: paramEdit.close()
                        }
                        FlatButton {
                            Layout.preferredWidth: 64
                            text: "ok"
                            onClicked: editorWindow.commitParamBinding()
                        }
                    }
                }
            }

            onClosing: Qt.callLater(function() { root.closeEditor(editorWindow.editorKey) })
        }
    }

    Window {
        id: pickerWindow
        property int location: 0
        property int target: -1
        property bool scanning: false
        property string channelFilter: ""

        modality: Qt.ApplicationModal
        flags: Qt.Dialog
        transientParent: root
        visible: false
        width: 440
        height: 520
        minimumWidth: 320
        minimumHeight: 240
        title: pickerWindow.location === 2
            ? "add bus insert"
            : (pickerWindow.location === 1 ? "add post-fader plugin" : "add pre-fader plugin")
        color: root.palette.window

        Timer {
            id: scanTimeout
            interval: 10000
            onTriggered: pickerWindow.scanning = false
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 6

            RowLayout {
                spacing: 6

                Label {
                    Layout.fillWidth: true
                    text: pickerWindow.title
                    font.bold: true
                    font.pixelSize: 12
                    elide: Text.ElideRight
                    color: root.textColor
                }

                FlatButton {
                    text: pickerWindow.scanning ? "scanning…" : "scan"
                    enabled: backend.connected && !pickerWindow.scanning
                    onClicked: {
                        pickerWindow.scanning = true
                        scanTimeout.restart()
                        backend.rescanPlugins()
                    }
                }

                FlatButton {
                    text: "close"
                    onClicked: pickerWindow.close()
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 4

                TextField {
                    id: pickerFilter
                    Layout.fillWidth: true
                    implicitHeight: 24
                    placeholderText: "filter..."
                }

                FlatButton {
                    text: "all"
                    enabled: pickerWindow.channelFilter !== ""
                    onClicked: pickerWindow.channelFilter = ""
                }
                FlatButton {
                    text: "mono"
                    enabled: pickerWindow.channelFilter !== "mono"
                    onClicked: pickerWindow.channelFilter = "mono"
                }
                FlatButton {
                    text: "stereo"
                    enabled: pickerWindow.channelFilter !== "stereo"
                    onClicked: pickerWindow.channelFilter = "stereo"
                }
            }

            Label {
                Layout.fillWidth: true
                text: root.filteredCatalog(pickerFilter.text, pickerWindow.channelFilter).length
                    + " / " + catalog.length + " plugins"
                color: root.mutedText
                font.pixelSize: 9
            }

            ListView {
                id: pickerList
                Layout.fillWidth: true
                Layout.fillHeight: true
                clip: true
                model: root.filteredCatalog(pickerFilter.text, pickerWindow.channelFilter)
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

                delegate: Rectangle {
                    id: pickerRow
                    width: ListView.view.width
                    height: 22
                    color: pickerMouse.containsMouse ? root.controlColor : "transparent"

                    Label {
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.leftMargin: 4
                        anchors.rightMargin: 8
                        text: modelData.name
                        elide: Text.ElideRight
                        color: root.textColor
                        font.pixelSize: 11
                    }

                    MouseArea {
                        id: pickerMouse
                        anchors.fill: parent
                        hoverEnabled: true
                        onClicked: {
                            backend.addPlugin(pickerWindow.location, pickerWindow.target, modelData.uri)
                            pickerWindow.close()
                        }
                    }
                }
            }
        }
    }

    Window {
        id: midiBindingsWindow
        title: "MIDI bindings"
        modality: Qt.ApplicationModal
        flags: Qt.Dialog
        transientParent: root
        visible: false
        width: 520
        height: 560
        color: palette.window

        function beginEditBinding(spec) {
            if (!spec) {
                return
            }
            bindingEdit.spec = spec
            bindingEditField.text = root.midiCcFor(spec.kind, spec.location, spec.target, spec.slot, spec.param)
            bindingEdit.open()
        }

        function commitEditBinding() {
            var spec = bindingEdit.spec
            if (!spec) {
                bindingEdit.close()
                return
            }
            root.midiBindFromField(spec.kind, spec.location, spec.target, spec.slot, spec.param, spec.min, spec.max, bindingEditField.text)
            bindingEdit.close()
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 6

            RowLayout {
                Layout.fillWidth: true
                Label {
                    Layout.fillWidth: true
                    text: "click a parameter to Edit / Learn / Clear"
                    color: root.mutedText
                    font.pixelSize: 11
                }
                FlatButton {
                    text: "clear all"
                    onClicked: backend.midiBindingsClear()
                }
                FlatButton {
                    text: "close"
                    onClicked: midiBindingsWindow.close()
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 4

                Label {
                    Layout.preferredWidth: 160
                    text: ""
                }
                Label {
                    Layout.preferredWidth: 56
                    text: "channel"
                    horizontalAlignment: Text.AlignHCenter
                    color: root.mutedText
                    font.pixelSize: 10
                }
                Repeater {
                    model: ["fader", "pan", "mute", "solo"]
                    Label {
                        Layout.preferredWidth: 56
                        text: modelData
                        horizontalAlignment: Text.AlignHCenter
                        color: root.mutedText
                        font.pixelSize: 10
                    }
                }
            }

            ScrollView {
                Layout.fillWidth: true
                Layout.fillHeight: true
                clip: true

                Column {
                    width: midiBindingsWindow.width - 40
                    spacing: 2

                    Repeater {
                        model: root.midiBindingRows()

                        RowLayout {
                            id: bindingRow
                            width: parent.width
                            spacing: 4

                            property var rowData: modelData

                            Label {
                                Layout.preferredWidth: 160
                                text: bindingRow.rowData.label
                                elide: Text.ElideRight
                                color: root.textColor
                                font.pixelSize: 11
                            }

                            FlatButton {
                                Layout.preferredWidth: 56
                                text: root.channelLabel(bindingRow.rowData.channel)
                                onClicked: channelMenu.popup()

                                Menu {
                                    id: channelMenu

                                    MenuItem {
                                        text: "any"
                                        onTriggered: bindingRow.rowData.is_bus
                                            ? backend.setBusMidiChannel(bindingRow.rowData.target, 16)
                                            : backend.setStripMidiChannel(bindingRow.rowData.target, 16)
                                    }
                                    MenuSeparator {}
                                    Repeater {
                                        model: 16
                                        MenuItem {
                                            text: "ch " + (index + 1)
                                            onTriggered: bindingRow.rowData.is_bus
                                                ? backend.setBusMidiChannel(bindingRow.rowData.target, index)
                                                : backend.setStripMidiChannel(bindingRow.rowData.target, index)
                                        }
                                    }
                                }
                            }

                            Repeater {
                                model: bindingRow.rowData.cells
                                BindingCell {
                                    Layout.preferredWidth: 56
                                    spec: modelData
                                    bindingHost: midiBindingsWindow
                                }
                            }
                        }
                    }
                }
            }
        }

        Dialog {
            id: bindingEdit
            property var spec: null
            title: "MIDI binding"
            modal: true
            anchors.centerIn: parent
            width: 280
            padding: 10

            contentItem: ColumnLayout {
                width: bindingEdit.availableWidth
                spacing: 8

                Label {
                    Layout.fillWidth: true
                    text: bindingEdit.spec ? bindingEdit.spec.label : ""
                    color: root.textColor
                    elide: Text.ElideRight
                    font.pixelSize: 11
                }

                TextField {
                    id: bindingEditField
                    Layout.fillWidth: true
                    implicitHeight: 24
                    placeholderText: "CC 0-127"
                    inputMethodHints: Qt.ImhDigitsOnly
                    onAccepted: midiBindingsWindow.commitEditBinding()
                }

                RowLayout {
                    Layout.fillWidth: true
                    Item { Layout.fillWidth: true }
                    FlatButton {
                        text: "cancel"
                        onClicked: bindingEdit.close()
                    }
                    FlatButton {
                        text: "ok"
                        onClicked: midiBindingsWindow.commitEditBinding()
                    }
                }
            }
        }
    }

    Dialog {
        id: loopDialog
        title: "loop settings"
        modal: true
        width: 320
        anchors.centerIn: parent
        padding: 10

        contentItem: ColumnLayout {
            width: loopDialog.availableWidth
            spacing: 8

            RowLayout {
                Layout.fillWidth: true
                Label {
                    Layout.fillWidth: true
                    text: "beats per bar"
                    color: root.textColor
                    font.pixelSize: 11
                }
                SpinBox {
                    from: 1
                    to: 16
                    value: Math.round(root.beatsPerBar())
                    onValueModified: backend.setBeatsPerBar(value)
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Label {
                    Layout.fillWidth: true
                    text: "loop unit"
                    color: root.textColor
                    font.pixelSize: 11
                }
                FlatButton {
                    text: root.transportUi().loop_bars ? "bars" : "beats"
                    onClicked: root.setTransportUi({ loop_bars: !root.transportUi().loop_bars })
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Label {
                    Layout.fillWidth: true
                    text: root.transportUi().loop_bars ? "loop bars" : "loop beats"
                    color: root.textColor
                    font.pixelSize: 11
                }
                SpinBox {
                    from: 1
                    to: 64
                    value: root.transportUi().loop_length
                    onValueModified: root.setTransportUi({ loop_length: value })
                }
            }

            FlatButton {
                text: "close"
                onClicked: loopDialog.close()
            }
        }
    }

    Dialog {
        id: missingDialog
        property var items: []
        title: "connections not restored"
        modal: true
        width: 460
        anchors.centerIn: parent
        padding: 10

        contentItem: ColumnLayout {
            width: missingDialog.availableWidth
            spacing: 8

            Label {
                Layout.fillWidth: true
                wrapMode: Text.WordWrap
                color: root.textColor
                font.pixelSize: 11
                text: "These saved connections could not be restored because the other end is not present:"
            }

            ListView {
                Layout.fillWidth: true
                Layout.preferredHeight: Math.min(missingDialog.items.length * 18 + 4, 260)
                clip: true
                model: missingDialog.items
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

                delegate: Label {
                    width: ListView.view.width
                    color: root.textColor
                    font.pixelSize: 11
                    elide: Text.ElideRight
                    text: {
                        var arrow = modelData.owner_kind === "strip" ? "\u2190" : "\u2192"
                        var owner = (modelData.owner_kind === "strip" ? "strip " : "bus ")
                            + "\"" + modelData.owner + "\""
                        return owner + " " + modelData.port_role + " " + arrow + " " + modelData.port
                    }
                }
            }

            FlatButton {
                Layout.alignment: Qt.AlignRight
                text: "ok"
                onClicked: {
                    missingDialog.close()
                    backend.report = ""
                }
            }
        }
    }

    Flickable {
        id: flick
        anchors.fill: parent
        contentWidth: content.width
        contentHeight: content.height
        clip: true
        flickableDirection: Flickable.HorizontalAndVerticalFlick
        interactive: !root.sliderHovered && !root.gripDragging

        Column {
            id: content
            padding: 6
            spacing: 6
            width: Math.max(implicitWidth, flick.width)

            Label { text: "channels"; font.bold: true; font.pixelSize: 11; color: root.mutedText }

            RowLayout {
                id: channelRow
                spacing: 4

                Repeater {
                    model: mix.strips

                    delegate: Rectangle {
                        id: stripCard
                        property var stripData: modelData
                        property string reorderKind: "strip"
                        property bool dragging: false
                        property real dragOffset: 0
                        Layout.fillHeight: true
                        Layout.preferredWidth: stripContent.implicitWidth + 12
                        implicitHeight: stripContent.implicitHeight + 12
                        color: root.panelColor
                        border.color: stripCard.dragging ? root.accentColor : root.borderColor
                        border.width: 1
                        z: stripCard.dragging ? 5 : 0
                        opacity: stripCard.dragging ? 0.85 : 1
                        transform: Translate { x: stripCard.dragOffset }

                        Menu {
                            id: stripMenu
                            MenuItem {
                                text: "Move left"
                                enabled: root.stripIndex(stripCard.stripData.id) > 0
                                onTriggered: root.moveStripBy(stripCard.stripData.id, -1)
                            }
                            MenuItem {
                                text: "Move right"
                                enabled: root.stripIndex(stripCard.stripData.id)
                                    < mix.strips.length - 1
                                onTriggered: root.moveStripBy(stripCard.stripData.id, 1)
                            }
                            MenuSeparator { }
                            MenuItem {
                                text: "Duplicate"
                                onTriggered: backend.duplicateStrip(stripCard.stripData.id)
                            }
                            MenuSeparator { }
                            MenuItem {
                                text: "Remove strip"
                                onTriggered: backend.removeStrip(stripCard.stripData.id)
                            }
                        }

                        ColumnLayout {
                            id: stripContent
                            anchors.fill: parent
                            anchors.margins: 6
                            spacing: 4

                            ReorderGrip {
                                Layout.fillWidth: true
                                card: stripCard
                                row: channelRow
                                kind: "strip"
                                onDropped: (index) => backend.moveStrip(stripCard.stripData.id, index)
                                onMenuRequested: stripMenu.popup()
                            }

                            NameEdit {
                                text: stripCard.stripData.name
                                Layout.preferredWidth: root.stripBody
                                onCommitted: (value) => backend.setStripName(stripCard.stripData.id, value)
                            }

                                    Knob {
                                        width: 22
                                        height: 22
                                        from: -20
                                        to: 24
                                        resetValue: 0
                                        entryUnit: "db"
                                        value: root.stripInputGain(stripCard.stripData.id)
                                        valueText: root.trimText(root.stripInputGain(stripCard.stripData.id))
                                        Layout.alignment: Qt.AlignHCenter
                                        onMoved: (value) => backend.setStripInputGain(stripCard.stripData.id, value)
                                    }

                            Label {
                                text: "trim"
                                color: root.mutedText
                                font.pixelSize: 8
                                Layout.alignment: Qt.AlignHCenter
                            }

                            PluginList {
                                location: 0
                                target: stripCard.stripData.id
                                plugins: stripCard.stripData.plugins
                                title: "pre"
                                Layout.preferredWidth: root.stripBody
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                Layout.fillHeight: true
                                spacing: 4

                                ColumnLayout {
                                    Layout.fillHeight: true
                                    spacing: 2

                                    Fader {
                                        Layout.fillHeight: true
                                        Layout.preferredHeight: 150
                                        Layout.alignment: Qt.AlignHCenter
                                        value: root.stripGain(stripCard.stripData.id)
                                        valueText: root.gainText(root.stripGain(stripCard.stripData.id))
                                        onMoved: (value) => backend.setStripGain(stripCard.stripData.id, value)
                                    }

                                    Knob {
                                        width: 22
                                        height: 22
                                        Layout.alignment: Qt.AlignHCenter
                                        entryUnit: "pan"
                                        value: root.stripPan(stripCard.stripData.id)
                                        valueText: root.panText(root.stripPan(stripCard.stripData.id))
                                        onMoved: (value) => backend.setStripPan(stripCard.stripData.id, value)
                                    }

                                    Label {
                                        text: "pan"
                                        color: root.mutedText
                                        font.pixelSize: 8
                                        Layout.alignment: Qt.AlignHCenter
                                    }
                                }

                                ColumnLayout {
                                    id: stripMeter
                                    Layout.fillHeight: true
                                    spacing: 2

                                    property string source: root.meterSourceFor(stripCard.stripData.id)

                                    StereoMeter {
                                        Layout.fillHeight: true
                                        Layout.preferredHeight: 150
                                        Layout.alignment: Qt.AlignHCenter
                                        levelL: root.stripLevel(stripCard.stripData.id, stripMeter.source, 0)
                                        levelR: root.stripLevel(stripCard.stripData.id, stripMeter.source, 1)
                                    }

                                    Label {
                                        text: root.formatDb(root.peakDb(stripCard.stripData.id, stripMeter.source))
                                        color: root.mutedText
                                        font.pixelSize: 8
                                        font.family: "monospace"
                                        Layout.alignment: Qt.AlignHCenter
                                    }

                                    FlatButton {
                                        text: root.meterSourceLabel(stripCard.stripData.id)
                                        implicitHeight: 18
                                        implicitWidth: 40
                                        Layout.alignment: Qt.AlignHCenter
                                        onClicked: root.cycleMeterSource(stripCard.stripData.id)
                                    }
                                }
                            }

                            PluginList {
                                location: 1
                                target: stripCard.stripData.id
                                plugins: stripCard.stripData.plugins_post
                                title: "post"
                                Layout.preferredWidth: root.stripBody
                            }

                            Column {
                                spacing: 2
                                Layout.preferredWidth: root.stripBody

                                Label {
                                    text: "sends"
                                    color: root.mutedText
                                    font.pixelSize: 8
                                }

                                Repeater {
                                    model: mix.buses

                                    delegate: Column {
                                        id: sendRow
                                        property var busData: modelData
                                        width: root.stripBody
                                        spacing: 1

                                        RowLayout {
                                            width: parent.width
                                            spacing: 2

                                            Label {
                                                Layout.fillWidth: true
                                                text: sendRow.busData.name
                                                elide: Text.ElideRight
                                                color: root.mutedText
                                                font.pixelSize: 8
                                            }

                                            Rectangle {
                                                id: delayBadge
                                                readonly property real delayMs: root.sendDelay(
                                                    stripCard.stripData.id, sendRow.busData.id)
                                                visible: delayMs > 0
                                                Layout.preferredWidth: 10
                                                Layout.preferredHeight: 10
                                                radius: 2
                                                color: root.accentColor

                                                Label {
                                                    anchors.centerIn: parent
                                                    text: "D"
                                                    color: "#ffffff"
                                                    font.pixelSize: 8
                                                    font.bold: true
                                                }

                                                HoverHandler { id: delayBadgeHover }
                                                ToolTip.visible: delayBadgeHover.hovered
                                                ToolTip.text: delayBadge.delayMs.toFixed(3) + " ms ("
                                                    + Math.round(delayBadge.delayMs
                                                        * (backend.sample_rate > 0 ? backend.sample_rate : 48000)
                                                        / 1000) + " samples)"
                                                ToolTip.delay: 300
                                            }
                                        }

                                        SendSlider {
                                            width: parent.width
                                            value: root.sendLevel(stripCard.stripData.id, sendRow.busData.id)
                                            delay: root.sendDelay(stripCard.stripData.id, sendRow.busData.id)
                                            formatValue: root.gainText
                                            onMoved: (v) => backend.setSendGain(
                                                stripCard.stripData.id,
                                                sendRow.busData.id,
                                                v)
                                            onDelayMoved: (ms) => backend.setSendDelay(
                                                stripCard.stripData.id,
                                                sendRow.busData.id,
                                                ms)
                                        }
                                    }
                                }
                            }

                            Row {
                                spacing: 3
                                Layout.alignment: Qt.AlignLeft
                                ToggleChip {
                                    text: "M"
                                    checked: root.stripMute(stripCard.stripData.id)
                                    onClicked: backend.setStripMute(stripCard.stripData.id, checked)
                                }
                                ToggleChip {
                                    text: "S"
                                    tint: "#f9a825"
                                    checked: root.stripSolo(stripCard.stripData.id)
                                    onClicked: backend.setStripSolo(stripCard.stripData.id, checked)
                                }
                            }
                        }
                    }
                }

                Item {
                    Layout.preferredWidth: 10
                    Layout.fillHeight: true
                    visible: mix.strips.length > 0 && mix.buses.length > 0
                }

                Repeater {
                    model: mix.buses

                    delegate: Rectangle {
                        id: busCard
                        property var busData: modelData
                        property string reorderKind: "bus"
                        readonly property bool reorderable: busCard.busData.id !== 0
                        property bool dragging: false
                        property real dragOffset: 0
                        Layout.fillHeight: true
                        Layout.preferredWidth: busContent.implicitWidth + 12
                        implicitHeight: busContent.implicitHeight + 12
                        color: root.panelColor
                        border.color: busCard.dragging ? root.accentColor : root.borderColor
                        border.width: 1
                        z: busCard.dragging ? 5 : 0
                        opacity: busCard.dragging ? 0.85 : 1
                        transform: Translate { x: busCard.dragOffset }

                        Menu {
                            id: busMenu
                            MenuItem {
                                text: "Move left"
                                enabled: busCard.reorderable && root.busIndex(busCard.busData.id) > 0
                                onTriggered: root.moveBusBy(busCard.busData.id, -1)
                            }
                            MenuItem {
                                text: "Move right"
                                enabled: busCard.reorderable
                                    && root.busIndex(busCard.busData.id) < mix.buses.length - 2
                                onTriggered: root.moveBusBy(busCard.busData.id, 1)
                            }
                            MenuSeparator { }
                            MenuItem {
                                text: "Mix to master"
                                checkable: true
                                checked: busCard.busData.mix_to_master === true
                                onTriggered: backend.setBusMixMaster(
                                    busCard.busData.id,
                                    !(busCard.busData.mix_to_master === true))
                            }
                            MenuItem {
                                text: "External return"
                                enabled: busCard.reorderable
                                checkable: true
                                checked: busCard.busData.return_inputs === true
                                onTriggered: backend.setBusReturn(
                                    busCard.busData.id,
                                    !(busCard.busData.return_inputs === true))
                            }
                            MenuSeparator { }
                            MenuItem {
                                text: "Duplicate"
                                enabled: busCard.reorderable
                                onTriggered: backend.duplicateBus(busCard.busData.id)
                            }
                            MenuSeparator { }
                            MenuItem {
                                text: "Remove bus"
                                enabled: busCard.reorderable
                                onTriggered: backend.removeBus(busCard.busData.id)
                            }
                        }

                        ColumnLayout {
                            id: busContent
                            anchors.fill: parent
                            anchors.margins: 6
                            spacing: 4

                            ReorderGrip {
                                Layout.fillWidth: true
                                card: busCard
                                row: channelRow
                                kind: "bus"
                                reorderEnabled: busCard.reorderable
                                onDropped: (index) => backend.moveBus(busCard.busData.id, index)
                                onMenuRequested: busMenu.popup()
                            }

                            NameEdit {
                                text: busCard.busData.name
                                Layout.preferredWidth: root.stripBody
                                onCommitted: (value) => backend.setBusName(busCard.busData.id, value)
                            }

                            Label {
                                visible: busCard.busData.mix_to_master === true
                                text: "→ master"
                                color: root.mutedText
                                font.pixelSize: 8
                                Layout.alignment: Qt.AlignLeft
                            }

                            RowLayout {
                                visible: busCard.busData.return_inputs === true
                                spacing: 6

                                ColumnLayout {
                                    spacing: 1

                                    Knob {
                                        width: 22
                                        height: 22
                                        Layout.alignment: Qt.AlignHCenter
                                        from: 0
                                        to: 2
                                        resetValue: 1
                                        entryUnit: "gain-db"
                                        value: root.busSendGain(busCard.busData.id)
                                        valueText: root.gainText(root.busSendGain(busCard.busData.id))
                                        onMoved: (value) => backend.setBusSendGain(busCard.busData.id, value)
                                    }

                                    Label {
                                        text: "SEND"
                                        color: root.mutedText
                                        font.pixelSize: 8
                                        Layout.alignment: Qt.AlignHCenter
                                    }
                                }

                                ColumnLayout {
                                    spacing: 1

                                    Knob {
                                        width: 22
                                        height: 22
                                        Layout.alignment: Qt.AlignHCenter
                                        from: 0
                                        to: 2
                                        resetValue: 1
                                        entryUnit: "gain-db"
                                        value: root.busReturnGain(busCard.busData.id)
                                        valueText: root.gainText(root.busReturnGain(busCard.busData.id))
                                        onMoved: (value) => backend.setBusReturnGain(busCard.busData.id, value)
                                    }

                                    Label {
                                        text: "RET"
                                        color: root.mutedText
                                        font.pixelSize: 8
                                        Layout.alignment: Qt.AlignHCenter
                                    }
                                }
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                Layout.fillHeight: true
                                spacing: 4

                                ColumnLayout {
                                    Layout.fillHeight: true
                                    spacing: 2

                                    Fader {
                                        Layout.fillHeight: true
                                        Layout.preferredHeight: 150
                                        Layout.alignment: Qt.AlignHCenter
                                        value: root.busGain(busCard.busData.id)
                                        valueText: root.gainText(root.busGain(busCard.busData.id))
                                        onMoved: (value) => backend.setBusGain(busCard.busData.id, value)
                                    }

                                    Item {
                                        Layout.preferredWidth: 1
                                        Layout.preferredHeight: busMeterValue.implicitHeight
                                    }

                                    ToggleChip {
                                        text: "M"
                                        implicitWidth: 18
                                        implicitHeight: 18
                                        Layout.alignment: Qt.AlignHCenter
                                        checked: root.busMute(busCard.busData.id)
                                        onClicked: backend.setBusMute(busCard.busData.id, checked)
                                    }
                                }

                                ColumnLayout {
                                    id: busMeter
                                    Layout.fillHeight: true
                                    spacing: 2

                                    property string source: root.busMeterSourceFor(busCard.busData.id)

                                    StereoMeter {
                                        Layout.fillHeight: true
                                        Layout.preferredHeight: 150
                                        Layout.alignment: Qt.AlignHCenter
                                        levelL: root.busLevel(busCard.busData.id, busMeter.source, 0)
                                        levelR: root.busLevel(busCard.busData.id, busMeter.source, 1)
                                    }

                                    Label {
                                        id: busMeterValue
                                        text: root.formatDb(root.busPeakDb(busCard.busData.id, busMeter.source))
                                        color: root.mutedText
                                        font.pixelSize: 8
                                        font.family: "monospace"
                                        Layout.alignment: Qt.AlignHCenter
                                    }

                                    FlatButton {
                                        text: root.busMeterSourceLabel(busCard.busData.id)
                                        implicitHeight: 18
                                        implicitWidth: 40
                                        Layout.alignment: Qt.AlignHCenter
                                        onClicked: root.cycleBusMeterSource(busCard.busData.id)
                                    }
                                }
                            }

                            PluginList {
                                location: 2
                                target: busCard.busData.id
                                plugins: busCard.busData.plugins
                                title: "inserts"
                                Layout.preferredWidth: root.stripBody
                            }
                        }
                    }
                }
            }
        }

        ScrollBar.vertical: ScrollBar { }
    }
}
