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
    minimumWidth: root.transportUi().autosize ? 320 : 760
    minimumHeight: root.transportUi().autosize ? appMenuBar.height + 220 : 480
    title: root.fileName(backend.session_path) + (backend.dirty ? " *" : "") + " — vibemix"

    property bool forceClose: false
    property var pendingAction: null
    property string sessionError: ""
    // Mixer button whose keyboard binding is being captured, or null.
    property var midiKeyLearnSpec: null
    // Column counts frozen while auto-fit is on (0 = not captured).
    property int autoFitStripColumns: 0
    property int autoFitBusColumns: 0
    // Card height shared by the bus cards so they line up with the strips.
    property real mixerCardHeight: 0

    Backend {
        id: backend
    }

    // Debug aid for scripts/gui-shot.py: `--snapshot-to=<png> [--snapshot-after=<ms>]`
    // saves the window contents after a delay and quits. Inert otherwise.
    Timer {
        id: snapshotTimer
        property string path: ""
        repeat: false
        onTriggered: {
            // `contentItem` itself has no QML engine; grab its largest visible
            // child (the mixer), not the hidden waiting-for-engine overlay.
            var target = null
            var best = 0
            for (var c = 0; c < root.contentItem.children.length; c++) {
                var child = root.contentItem.children[c]
                if (child.visible && child.width * child.height > best) {
                    best = child.width * child.height
                    target = child
                }
            }
            console.log("snapshot state: connected=" + backend.connected + " catalog_ready="
                + backend.catalog_ready + " strips=" + mix.strips.length + " buses=" + mix.buses.length)
            if (!target || !target.grabToImage(function (result) {
                result.saveToFile(snapshotTimer.path)
                Qt.quit()
            })) {
                console.log("snapshot failed")
                Qt.quit()
            }
        }
        Component.onCompleted: {
            var args = Qt.application.arguments
            var delay = 8000
            for (var i = 0; i < args.length; i++) {
                if (args[i].indexOf("--snapshot-to=") === 0)
                    snapshotTimer.path = args[i].substring("--snapshot-to=".length)
                else if (args[i].indexOf("--snapshot-after=") === 0)
                    delay = parseInt(args[i].substring("--snapshot-after=".length))
            }
            if (snapshotTimer.path !== "") {
                snapshotTimer.interval = delay
                snapshotTimer.start()
            }
        }
    }

    Connections {
        target: backend
        function onPortsChanged() { root.ensurePatchClients() }
    }

    // Auto-fit (Options menu): while enabled, drive the window size from the
    // mixer content, clamped to the screen's available area. Inactive otherwise
    // so the window stays freely resizable, and while maximized.
    // The window follows the mixer content, but only once the content size has
    // been stable for a moment. Binding the window size live sent every
    // intermediate layout (for example a card removed before the grid had been
    // re-wrapped, which briefly doubled the height) to the window manager, so the
    // window visibly flashed another geometry.
    readonly property bool autoFitActive:
        root.transportUi().autosize && root.visibility !== Window.Maximized
    readonly property int autoFitTargetWidth:
        Math.min(root.autoFitWidth(), Screen.desktopAvailableWidth)
    readonly property int autoFitTargetHeight:
        Math.min(appMenuBar.height + root.autoFitHeight(), Screen.desktopAvailableHeight)
    onAutoFitTargetWidthChanged: if (autoFitActive) autoFitTimer.restart()
    onAutoFitTargetHeightChanged: if (autoFitActive) autoFitTimer.restart()
    onAutoFitActiveChanged: if (autoFitActive) autoFitTimer.restart()
    Timer {
        id: autoFitTimer
        interval: 60
        onTriggered: {
            if (!root.autoFitActive) return
            if (root.width !== root.autoFitTargetWidth) root.width = root.autoFitTargetWidth
            if (root.height !== root.autoFitTargetHeight) root.height = root.autoFitTargetHeight
        }
    }

    readonly property int stripBody: 76
    readonly property int stripCardWidth: stripBody + 12
    readonly property int busCardWidth: stripBody + 12
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

    // The strip and bus repeaters are driven by id-only models, so a snapshot
    // only creates or destroys the cards that were actually added or removed.
    // (Handing a fresh array to a Repeater re-creates every card, which took
    // minutes with 64 strips x 32 buses.) Cards look their data up by id; the
    // lookup tables are plain objects mutated in place and announced by bumping
    // `dataRevision`, so bindings re-run once per snapshot.
    ListModel { id: stripIdModel }
    ListModel { id: busIdModel }
    property var dataIndex: ({ strips: ({}), buses: ({}) })
    // JSON text of each entry as last stored, so an unchanged strip or bus keeps
    // the very same object and none of its bindings fire.
    property var dataText: ({ strips: ({}), buses: ({}) })
    property int dataRevision: 0
    function stripAt(id) { return dataRevision >= 0 ? dataIndex.strips[id] : undefined }
    function busAt(id) { return dataRevision >= 0 ? dataIndex.buses[id] : undefined }

    // Makes `model` hold exactly `ids` (role `role`) in order, touching only
    // the entries that changed: removes the gone, moves the reordered, inserts
    // the new.
    function syncIdModel(model, role, ids) {
        var wanted = {}
        for (var k = 0; k < ids.length; k++) wanted[ids[k]] = true
        for (var i = model.count - 1; i >= 0; i--)
            if (!wanted[model.get(i)[role]]) model.remove(i)
        for (var j = 0; j < ids.length; j++) {
            if (j < model.count && model.get(j)[role] === ids[j]) continue
            var found = -1
            for (var m = j + 1; m < model.count; m++) {
                if (model.get(m)[role] === ids[j]) { found = m; break }
            }
            if (found >= 0) {
                model.move(found, j, 1)
            } else {
                var entry = {}
                entry[role] = ids[j]
                model.insert(j, entry)
            }
        }
    }

    function applySnapshot(parsed) {
        var strips = dataIndex.strips
        var buses = dataIndex.buses
        var stripIds = []
        var busIds = []
        for (var s = 0; s < parsed.strips.length; s++) {
            var strip = parsed.strips[s]
            var stripText = JSON.stringify(strip)
            if (dataText.strips[strip.id] !== stripText || !strips[strip.id]) {
                strips[strip.id] = strip
                dataText.strips[strip.id] = stripText
            }
            stripIds.push(strip.id)
        }
        for (var b = 0; b < parsed.buses.length; b++) {
            var bus = parsed.buses[b]
            var busText = JSON.stringify(bus)
            if (dataText.buses[bus.id] !== busText || !buses[bus.id]) {
                buses[bus.id] = bus
                dataText.buses[bus.id] = busText
            }
            busIds.push(bus.id)
        }
        mix = parsed
        syncIdModel(stripIdModel, "sid", stripIds)
        syncIdModel(busIdModel, "bid", busIds)
        // Entries of removed items were kept until their cards were destroyed.
        var liveStrips = {}
        var liveBuses = {}
        for (var i = 0; i < stripIds.length; i++) liveStrips[stripIds[i]] = true
        for (var j = 0; j < busIds.length; j++) liveBuses[busIds[j]] = true
        for (var key in strips) {
            if (!liveStrips[key]) {
                delete strips[key]
                delete dataText.strips[key]
            }
        }
        for (var bkey in buses) {
            if (!liveBuses[bkey]) {
                delete buses[bkey]
                delete dataText.buses[bkey]
            }
        }
        dataRevision++
    }
    property var levels: ({ strips: {}, buses: {} })
    property var catalog: []
    property var recorderRows: []
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
    function dropIndexFor(row, card, dx, dy, kind) {
        var cx = card.x + card.width / 2 + dx
        var cy = card.y + card.height / 2 + dy
        var result = 0
        var kids = row.children
        for (var i = 0; i < kids.length; i++) {
            var child = kids[i]
            if (child === card || child.reorderKind !== kind) continue
            if (child.y + child.height <= cy) {
                // entirely above the drop point
                result++
            } else if (child.y < cy) {
                // same row as the drop point: count only what is to its left
                if (child.x + child.width / 2 < cx) result++
            }
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

    // --- Patchbay -----------------------------------------------------------
    function parsePorts(text) {
        try {
            var parsed = JSON.parse(text)
            return Array.isArray(parsed) ? parsed : []
        } catch (error) {
            return []
        }
    }
    readonly property var portCache: root.parsePorts(backend.ports)
    function portList() { return root.portCache }
    function portClients() {
        var ports = root.portList()
        var seen = ({})
        var clients = []
        for (var i = 0; i < ports.length; i++) {
            var client = ports[i].client
            if (!seen[client]) {
                seen[client] = true
                clients.push(client)
            }
        }
        var engine = root.vibemixClient()
        clients.sort(function(a, b) {
            if (a === engine) return -1
            if (b === engine) return 1
            if (a === "system") return -1
            if (b === "system") return 1
            return a.localeCompare(b)
        })
        return clients
    }
    function vibemixClient() {
        var ports = root.portList()
        for (var i = 0; i < ports.length; i++) {
            if (ports[i].role) return ports[i].client
        }
        return ""
    }
    function portsFor(client, wantOutput, type) {
        var ports = root.portList()
        var out = []
        for (var i = 0; i < ports.length; i++) {
            var port = ports[i]
            if (port.client !== client || port.type !== type) continue
            if (wantOutput ? port.output !== true : port.input !== true) continue
            out.push(port)
        }
        return out
    }
    function portConnected(sourceName, sink) {
        return sink && Array.isArray(sink.connections)
            && sink.connections.indexOf(sourceName) >= 0
    }
    /// Current level (0..1 linear peak) of a vibemix port, from the existing
    /// strip/bus meter taps. External ports have no tap and report 0.
    function portLevel(port) {
        if (!port || !port.role) return 0
        var channel = port.role.channel === "r" ? 1 : 0
        if (port.role.kind === "strip_in") return root.stripLevel(port.role.id, "in", channel)
        if (port.role.kind === "bus_out") return root.busLevel(port.role.id, "out", channel)
        if (port.role.kind === "bus_ret") return root.busLevel(port.role.id, "ret", channel)
        return 0
    }
    function levelFraction(level) {
        if (level <= 0.00001) return 0
        var db = 20 * Math.log(level) / Math.LN10
        return Math.max(0, Math.min(1, (db + 60) / 60))
    }
    function levelColor(level) {
        var f = root.levelFraction(level)
        if (f <= 0) return "transparent"
        if (f < 0.7) return Qt.rgba(f / 0.7, 1, 0, 1)
        return Qt.rgba(1, 1 - (f - 0.7) / 0.3, 0, 1)
    }
    function portShortLabel(port) {
        if (!port || !port.role) return port ? port.short : ""
        if (port.role.kind === "midi_in") return "MIDI in"
        var channel = port.role.channel === "l" ? " L" : (port.role.channel === "r" ? " R" : "")
        var kind = port.role.kind === "strip_in" ? "in"
            : (port.role.kind === "bus_out" ? "out" : "ret")
        return port.role.label + " " + kind + channel
    }
    function patchCanConnect(source, sink) {
        return (source.role !== undefined && source.role !== null)
            || (sink.role !== undefined && sink.role !== null)
    }
    function patchSources(client) {
        var sources = root.portsFor(client, true, "audio")
        if (!patchWindow.onlyConnected) return sources
        var sinks = root.portsFor(patchWindow.sinkClient, false, "audio")
        return sources.filter(function(source) {
            for (var i = 0; i < sinks.length; i++) {
                if (root.portConnected(source.name, sinks[i])) return true
            }
            return false
        })
    }
    function patchSinks(client) {
        var sinks = root.portsFor(client, false, "audio")
        if (!patchWindow.onlyConnected) return sinks
        var sources = root.portsFor(patchWindow.sourceClient, true, "audio")
        return sinks.filter(function(sink) {
            for (var i = 0; i < sources.length; i++) {
                if (root.portConnected(sources[i].name, sink)) return true
            }
            return false
        })
    }
    function setPortConnection(source, sink, connect) {
        if (!source || !sink) return
        if (connect) backend.connectPorts(source.name, sink.name)
        else backend.disconnectPorts(source.name, sink.name)
    }
    function togglePortCell(source, sink, connect) {
        root.setPortConnection(source, sink, connect)
        patchRefreshTimer.restart()
    }
    function ensurePatchClients() {
        var clients = root.portClients()
        if (clients.length === 0) return
        var engine = root.vibemixClient()
        if (clients.indexOf(patchWindow.sourceClient) < 0) {
            // Prefer the device/app with the most audio outputs (the hardware
            // client on PipeWire; there is no "system" client there).
            var best = ""
            var bestCount = 0
            var engineCount = 0
            for (var i = 0; i < clients.length; i++) {
                var client = clients[i]
                var count = root.portsFor(client, true, "audio").length
                if (client === engine) {
                    engineCount = count
                } else if (count > bestCount) {
                    bestCount = count
                    best = client
                }
            }
            patchWindow.sourceClient = best !== ""
                ? best : (engine !== "" ? engine : clients[0])
        }
        if (clients.indexOf(patchWindow.sinkClient) < 0) {
            patchWindow.sinkClient = engine !== "" ? engine : clients[clients.length - 1]
        }
    }
    function midiInputPort() {
        var ports = root.portList()
        for (var i = 0; i < ports.length; i++) {
            if (ports[i].role && ports[i].role.kind === "midi_in") return ports[i]
        }
        return null
    }
    function midiSources() {
        var ports = root.portList()
        var out = []
        for (var i = 0; i < ports.length; i++) {
            if (ports[i].type === "midi" && ports[i].output === true) out.push(ports[i])
        }
        out.sort(function(a, b) {
            if (a.client !== b.client) return a.client.localeCompare(b.client)
            return a.short.localeCompare(b.short)
        })
        return out
    }
    function midiConnected(port) {
        var input = root.midiInputPort()
        return input ? root.portConnected(port.name, input) : false
    }
    function toggleMidiSource(port, connect) {
        var input = root.midiInputPort()
        if (!input) return
        root.setPortConnection(port, input, connect)
        midiRefreshTimer.restart()
    }

    // --- Mixer layout -------------------------------------------------------
    function layoutSettings() {
        var layout = mix.layout
        return layout ? layout : ({ strips_per_row: 0, buses_per_row: 0 })
    }
    function layoutColumns(kind) {
        var layout = root.layoutSettings()
        return kind === "strip" ? layout.strips_per_row : layout.buses_per_row
    }
    function paneColumns(kind) {
        // Count the cards that exist (the id models drive the repeaters), not
        // the snapshot, so the column count and the cards change together.
        var count = kind === "strip" ? stripIdModel.count : busIdModel.count
        var fixed = root.layoutColumns(kind)
        if (root.transportUi().autosize) {
            // While auto-fit is on the window is sized to the content, so the
            // column count must not depend on the pane width (that would be a
            // binding loop). Use the fixed setting or the captured wrap, and
            // never reserve empty columns.
            var columns = fixed > 0
                ? fixed
                : (kind === "strip" ? root.autoFitStripColumns : root.autoFitBusColumns)
            if (!columns || columns <= 0) columns = kind === "strip" ? 6 : 4
            return Math.max(1, Math.min(Math.max(1, count), columns))
        }
        if (fixed && fixed > 0) return Math.min(32, fixed)
        var card = kind === "strip" ? root.stripCardWidth : root.busCardWidth
        var availableWidth = kind === "strip" ? stripsFlick.width : busesFlick.width
        return Math.max(1, Math.floor((availableWidth - 8 + 4) / (card + 4)))
    }
    function canAddStrip() {
        var capacity = mix.strip_capacity
        return !capacity || mix.strips.length < capacity
    }
    function canAddBus() {
        var capacity = mix.bus_capacity
        return !capacity || mix.buses.length < capacity
    }
    function autoFitWidth() {
        var strips = Math.max(160, stripsGrid.implicitWidth + 12)
        var buses = Math.max(140, busesGrid.implicitWidth + 12)
        return Math.max(320, strips + buses + 12)
    }
    function autoFitHeight() {
        var strips = stripsGrid.implicitHeight + 24 + 10
        var buses = busesGrid.implicitHeight + 24 + 10
        return Math.max(220, Math.max(strips, buses))
    }
    function toggleAutoFit() {
        var on = !root.transportUi().autosize
        if (on) {
            // Preserve the wrap the user is looking at.
            root.autoFitStripColumns = Math.max(1, stripsGrid.columns)
            root.autoFitBusColumns = Math.max(1, busesGrid.columns)
        }
        root.setTransportUi({ autosize: on })
    }
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
    function sendPreFader(stripId, busId) {
        var entry = values.strips[stripId]
        if (!entry || !entry.send_pre) return false
        return entry.send_pre[busId] === true
    }
    // Every sidechain-capable plugin, as a send destination on each strip.
    function sidechainDestinations() {
        var out = []
        function addAll(list, location, target, hostName, hostStripId) {
            if (!list) return
            for (var i = 0; i < list.length; i++) {
                var p = list[i]
                if (p.has_sidechain === true && p.sidechain_enabled === true) {
                    out.push({
                        location: location,
                        target: target,
                        slot: p.slot,
                        name: p.name,
                        hostName: hostName,
                        hostStripId: hostStripId
                    })
                }
            }
        }
        for (var s = 0; s < mix.strips.length; s++) {
            var strip = mix.strips[s]
            addAll(strip.plugins, 0, strip.id, strip.name, strip.id)
            addAll(strip.plugins_post, 1, strip.id, strip.name, strip.id)
        }
        for (var b = 0; b < mix.buses.length; b++) {
            var bus = mix.buses[b]
            addAll(bus.plugins, 2, bus.id, bus.name, -1)
        }
        return out
    }
    function sidechainEntry(location, target, slot, stripId) {
        if (!values.sidechain) return undefined
        var map = values.sidechain[location + "-" + target + "-" + slot]
        return map ? map[stripId] : undefined
    }
    function sidechainGain(location, target, slot, stripId) {
        var entry = root.sidechainEntry(location, target, slot, stripId)
        return entry && entry.gain !== undefined ? entry.gain : 0
    }
    function sidechainPreFader(location, target, slot, stripId) {
        var entry = root.sidechainEntry(location, target, slot, stripId)
        return entry ? entry.pre_fader !== false : true
    }
    function pluginFailed(entry) {
        return entry.status === "failed" || entry.status === "unsupported"
    }
    function pluginStatusText(entry) {
        var reason = entry.reason ? entry.reason : ""
        if (entry.status === "failed") return reason.length > 0 ? reason : "crashed"
        if (entry.status === "unsupported") return reason.length > 0 ? reason : "unsupported"
        return ""
    }
    function filteredCatalog(query, opts) {
        var needle = query ? query.toLowerCase() : ""
        // When both channels are enabled there is no channel restriction (so
        // entries with an unknown channel count stay visible); narrowing to one
        // hides the unknown ones.
        var bothChannels = opts.showMono && opts.showStereo
        return catalog.filter(function(entry) {
            if (!opts.showFailed && root.pluginFailed(entry)) return false
            if (entry.format === "lv2" && !opts.showLv2) return false
            if (entry.format === "vst3" && !opts.showVst3) return false
            if (entry.format === "clap" && !opts.showClap) return false
            if (opts.scOnly && entry.has_sidechain !== true) return false
            if (!bothChannels) {
                var channel = entry.outputs
                if (channel !== 1 && channel !== 2) return false
                if (!(channel === 1 && opts.showMono) && !(channel === 2 && opts.showStereo)) {
                    return false
                }
            }
            if (!needle) return true
            return entry.name.toLowerCase().indexOf(needle) >= 0
                || entry.uri.toLowerCase().indexOf(needle) >= 0
                || (entry.vendor || "").toLowerCase().indexOf(needle) >= 0
                || (entry.category || "").toLowerCase().indexOf(needle) >= 0
        })
    }
    // Muted second line of a picker row: vendor · category.
    function pluginSubtitle(entry) {
        var parts = []
        if (entry.vendor) parts.push(entry.vendor)
        if (entry.category) parts.push(entry.category)
        return parts.join(" \u00b7 ")
    }
    function formatLabel(entry) {
        if (entry.format === "vst3") return "VST3"
        if (entry.format === "clap") return "CLAP"
        return "LV2"
    }
    function channelText(entry) {
        if (!entry.inputs && !entry.outputs) return ""
        return entry.inputs + " in / " + entry.outputs + " out"
    }
    // Label/value rows for the details panel; empty values are omitted.
    function pluginDetails(entry) {
        if (!entry) return []
        var rows = []
        function add(label, value) {
            if (value === undefined || value === null) return
            var text = String(value)
            if (text.length === 0) return
            rows.push({ label: label, value: text })
        }
        add("Vendor", entry.vendor)
        add("Category", entry.category)
        add("Version", entry.version)
        add("Format", root.formatLabel(entry))
        add("Channels", root.channelText(entry))
        add("MIDI", entry.midi ? "yes" : "")
        add("Sidechain", entry.has_sidechain ? "yes" : "")
        add("Homepage", entry.homepage)
        add("License", entry.license)
        add("ID", entry.uri)
        add("Bundle", entry.bundle)
        if (entry.status && entry.status !== "ok") add("Status", root.pluginStatusText(entry))
        return rows
    }
    function localPath(url) {
        var text = String(url)
        if (text.indexOf("file://") === 0) text = text.slice(7)
        try { text = decodeURIComponent(text) } catch (e) {}
        return text
    }

    function transportUi() {
        var t = mix.transport_ui
        return t ? t : ({ beats: true, bpm: true, stability: true, time: false, loop_indicator: false, loop_bars: false, loop_length: 8, autosize: false })
    }
    function setTransportUi(patch) {
        var t = root.transportUi()
        var next = {
            beats: t.beats, bpm: t.bpm, stability: t.stability, time: t.time,
            loop_indicator: t.loop_indicator,
            loop_bars: t.loop_bars, loop_length: t.loop_length,
            autosize: t.autosize
        }
        for (var k in patch) next[k] = patch[k]
        backend.setTransportUi(JSON.stringify(next))
    }
    function clockInfo() { return JSON.parse(backend.clock) }
    function beatsPerBar() {
        var bpb = root.clockInfo().beats_per_bar
        return bpb > 0 ? bpb : 4
    }
    // Number of beats in a bar, as an integer, for grouping the loop indicator.
    function loopBeatsPerBar() {
        return Math.max(1, Math.round(root.beatsPerBar()))
    }
    // MIDI clock stability: tempo jitter (std-dev of instantaneous BPM) and the
    // number of glitchy (missed/extra) pulses in the recent window.
    function stabilityText() {
        var c = root.clockInfo()
        if (!c.locked) return "±— BPM"
        var jitter = (c.jitter_bpm !== undefined) ? c.jitter_bpm : 0
        var glitches = (c.glitches !== undefined) ? c.glitches : 0
        var text = "±" + Number(jitter).toFixed(2) + " BPM"
        if (glitches > 0) text += " · " + glitches + (glitches === 1 ? " glitch" : " glitches")
        return text
    }
    function stabilityColor() {
        var c = root.clockInfo()
        if (!c.locked) return root.mutedText
        var jitter = (c.jitter_bpm !== undefined) ? c.jitter_bpm : 0
        var glitches = (c.glitches !== undefined) ? c.glitches : 0
        if (glitches > 0 || jitter >= 5.0) return "#e53935"
        if (jitter >= 2.0) return "#f9a825"
        return "#43a047"
    }
    function stabilityTooltip() {
        var c = root.clockInfo()
        if (!c.locked) return "waiting for MIDI clock"
        return "MIDI clock tempo jitter and glitchy (missed/extra) pulses over the last ~4 beats"
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
    function recorderInfo() {
        try { return JSON.parse(backend.recorder) } catch (e) { return ({ state: "idle", progress: 0 }) }
    }
    function loopList() {
        try {
            var value = JSON.parse(backend.loops)
            return Array.isArray(value) ? value : []
        } catch (e) { return [] }
    }
    function loopById(id) {
        var loops = root.loopList()
        for (var i = 0; i < loops.length; i++) if (loops[i].id === id) return loops[i]
        return null
    }
    function recorderSettings() {
        var r = mix.recorder || ({})
        return {
            sources: r.sources || [],
            bit_depth: (r.bit_depth !== undefined) ? r.bit_depth : 24,
            save_dir: r.save_dir || "",
            auto_latency: r.auto_latency !== false,
            transient: r.transient !== false,
            nudge_samples: r.nudge_samples || 0,
            crossfade_ms: r.crossfade_ms || 0
        }
    }
    function updateRecorderSetting(key, value) {
        var settings = root.recorderSettings()
        settings[key] = value
        backend.setRecorderSettings(JSON.stringify(settings))
    }
    function sendRecorderAction(action) {
        backend.recorderAction(JSON.stringify(action))
    }
    function loopFolder() {
        var folder = root.recorderSettings().save_dir
        if (folder && folder.length > 0) return folder
        return root.recorderInfo().loops_dir || ""
    }
    function loopFolderUrl() {
        var folder = root.loopFolder()
        return folder.length > 0 ? "file://" + folder : "file:///"
    }
    function openRecorder() {
        var selected = root.recorderSettings().sources
        var rows = []
        function selectedTap(kind, id) {
            for (var i = 0; i < selected.length; i++) {
                if (selected[i].kind === kind && selected[i].id === id) return selected[i].tap
            }
            return null
        }
        var strips = mix.strips || []
        for (var s = 0; s < strips.length; s++) {
            var stripTap = selectedTap("strip", strips[s].id)
            rows.push({
                kind: "strip", id: strips[s].id, name: strips[s].name || ("strip " + strips[s].id),
                checked: stripTap !== null, tap: stripTap || "post_fader",
                taps: ["input", "pre_fader", "post_fader"]
            })
        }
        var buses = mix.buses || []
        for (var b = 0; b < buses.length; b++) {
            if (buses[b].id === 0) continue
            var busTap = selectedTap("bus", buses[b].id)
            rows.push({
                kind: "bus", id: buses[b].id, name: buses[b].name || ("bus " + buses[b].id),
                checked: busTap !== null, tap: "output", taps: ["output"]
            })
        }
        root.recorderRows = rows
        recorderDialog.open()
    }
    function applyRecorderRows() {
        var sources = []
        for (var i = 0; i < root.recorderRows.length; i++) {
            var row = root.recorderRows[i]
            if (row.checked) sources.push({ kind: row.kind, id: row.id, tap: row.tap })
        }
        var settings = root.recorderSettings()
        settings.sources = sources
        backend.setRecorderSettings(JSON.stringify(settings))
    }
    function setRecorderRow(index, key, value) {
        var rows = root.recorderRows.slice()
        rows[index][key] = value
        root.recorderRows = rows
        root.applyRecorderRows()
    }
    function loopDescription(slot) {
        var seconds = slot.sample_rate > 0 ? slot.frames / slot.sample_rate : 0
        var length = slot.loop_beats > 0 ? (slot.loop_beats + " beats") : (seconds.toFixed(2) + " s")
        return length + " · " + Math.round(slot.bpm || 0) + " BPM"
    }
    function loopNudgeText(slot) {
        var ms = slot.sample_rate > 0 ? slot.nudge_samples * 1000 / slot.sample_rate : 0
        return slot.nudge_samples + " smp (" + (ms >= 0 ? "+" : "") + ms.toFixed(2) + " ms)"
    }
    function nudgeLoop(delta) {
        var slot = root.loopById(loopEditor.slotId)
        if (!slot) return
        var manual = loopEditor.displayManual() + delta
        loopEditor.dragManual = manual
        loopEditor.applying = true
        root.sendRecorderAction({ action: "set_nudge", slot: slot.id, manual: manual })
        editorWave.requestPaint()
    }
    function startLoop(slot, quantized) {
        root.sendRecorderAction({ action: "loop_play", slot: slot.id, quantized: quantized })
    }
    function stopLoop(slot) {
        root.sendRecorderAction({ action: "loop_stop", slot: slot.id })
    }
    function toggleLoopMute(slot) {
        root.sendRecorderAction({ action: "set_loop_mute", slot: slot.id, mute: !slot.mute })
    }
    function saveLoop(slot, bitDepth) {
        root.sendRecorderAction({ action: "loop_save", slot: slot.id, path: null, bit_depth: bitDepth })
    }
    function renameLoop(slot, name) {
        root.sendRecorderAction({ action: "loop_rename", slot: slot.id, name: name })
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
    function midiBindingFor(kind, location, target, slot, param) {
        var name = root.midiKindName(kind)
        var list = mix.midi_bindings || []
        for (var i = 0; i < list.length; ++i) {
            var b = list[i]
            if (b.target === name && b.location === location && b.target_id === target
                    && b.slot === slot && b.param === param) {
                return b
            }
        }
        return null
    }
    function midiCcFor(kind, location, target, slot, param) {
        var b = root.midiBindingFor(kind, location, target, slot, param)
        return b && b.cc !== undefined && b.cc !== null ? String(b.cc) : ""
    }
    function keyFor(kind, location, target, slot, param) {
        var b = root.midiBindingFor(kind, location, target, slot, param)
        return b && b.key ? b.key : ""
    }
    function bindingLabel(kind, location, target, slot, param) {
        var cc = root.midiCcFor(kind, location, target, slot, param)
        var key = root.keyFor(kind, location, target, slot, param)
        if (cc.length > 0 && key.length > 0) return cc + " / " + key
        if (cc.length > 0) return cc
        if (key.length > 0) return key
        return "—"
    }
    function isButtonKind(kind) {
        return kind === 2 || kind === 3 || kind === 5 || kind === 9
    }
    // Qt-standard key names so `Shortcut.sequence` parses them back.
    // Modifier keys deliver their own key-down event; ignore them so a combo
    // is captured once the real key arrives.
    function isModifierKey(key) {
        return key === Qt.Key_Shift || key === Qt.Key_Control || key === Qt.Key_Alt
            || key === Qt.Key_Meta || key === Qt.Key_AltGr || key === Qt.Key_CapsLock
            || key === Qt.Key_NumLock
    }
    function keyName(event) {
        if (root.isModifierKey(event.key)) {
            return ""
        }
        switch (event.key) {
        case Qt.Key_Escape: return "Esc"
        case Qt.Key_Space: return "Space"
        case Qt.Key_Return: return "Return"
        case Qt.Key_Enter: return "Enter"
        case Qt.Key_Tab: return "Tab"
        case Qt.Key_Backtab: return "Backtab"
        case Qt.Key_Backspace: return "Backspace"
        case Qt.Key_Delete: return "Del"
        case Qt.Key_Insert: return "Ins"
        case Qt.Key_Home: return "Home"
        case Qt.Key_End: return "End"
        case Qt.Key_PageUp: return "PgUp"
        case Qt.Key_PageDown: return "PgDown"
        case Qt.Key_Up: return "Up"
        case Qt.Key_Down: return "Down"
        case Qt.Key_Left: return "Left"
        case Qt.Key_Right: return "Right"
        default: break
        }
        if (event.key >= Qt.Key_F1 && event.key <= Qt.Key_F35) {
            return "F" + (event.key - Qt.Key_F1 + 1)
        }
        if (event.text && event.text.length > 0) {
            return event.text.toUpperCase()
        }
        return "Key" + event.key
    }
    function keyCombo(event) {
        var name = root.keyName(event)
        if (name.length === 0) {
            return ""
        }
        var parts = []
        if (event.modifiers & Qt.ControlModifier) parts.push("Ctrl")
        if (event.modifiers & Qt.AltModifier) parts.push("Alt")
        if (event.modifiers & Qt.ShiftModifier) parts.push("Shift")
        if (event.modifiers & Qt.MetaModifier) parts.push("Meta")
        parts.push(name)
        return parts.join("+")
    }
    function textEditing() {
        var item = root.activeFocusItem
        return !!item && item.cursorPosition !== undefined
    }
    function beginKeyLearn(spec) {
        if (spec) {
            root.midiKeyLearnSpec = spec
        }
    }
    // Captures the next key: Esc cancels, Backspace/Delete clears the binding.
    function handleKeyCapture(event) {
        if (root.midiKeyLearnSpec === null) {
            return
        }
        // Wait for a real key: modifier presses are ignored.
        if (root.isModifierKey(event.key)) {
            return
        }
        event.accepted = true
        if (event.key === Qt.Key_Escape) {
            root.midiKeyLearnSpec = null
            return
        }
        var spec = root.midiKeyLearnSpec
        if (event.key === Qt.Key_Backspace || event.key === Qt.Key_Delete) {
            backend.midiUnbind(spec.kind, spec.location, spec.target, spec.slot, spec.param)
            root.midiKeyLearnSpec = null
            return
        }
        backend.keyBind(spec.kind, spec.location, spec.target, spec.slot, spec.param,
            root.keyCombo(event))
        root.midiKeyLearnSpec = null
    }
    // Fires a bound button for a key press that reaches the main content item.
    function handleGlobalKey(event) {
        if (root.midiKeyLearnSpec !== null || root.textEditing()) {
            return
        }
        var combo = root.keyCombo(event)
        if (combo.length === 0) {
            return
        }
        var list = mix.midi_bindings || []
        for (var i = 0; i < list.length; ++i) {
            if (list[i].key === combo) {
                backend.keyTrigger(combo)
                event.accepted = true
                return
            }
        }
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
                enabled: backend.connected && root.canAddStrip()
                onTriggered: backend.addStrip()
            }
            MenuItem {
                text: "Add bus"
                enabled: backend.connected && root.canAddBus()
                onTriggered: backend.addBus()
            }
        }

        Menu {
            title: "Options"
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
            MenuSeparator { }
            MenuItem {
                text: "Auto-fit window to content"
                checkable: true
                checked: root.transportUi().autosize
                onTriggered: root.toggleAutoFit()
            }
            MenuSeparator { }
            MenuItem {
                text: "Layout…"
                onTriggered: layoutDialog.open()
            }
            MenuSeparator { }
            MenuItem {
                text: "Patchbay…"
                onTriggered: patchWindow.show()
            }
            MenuItem {
                text: "MIDI input…"
                onTriggered: midiWindow.show()
            }
            MenuSeparator { }
            MenuItem {
                text: "Recorder…"
                enabled: backend.connected
                onTriggered: root.openRecorder()
            }
            MenuItem {
                text: "Loops…"
                enabled: backend.connected
                onTriggered: loopsWindow.show()
            }
            MenuSeparator { }
            MenuItem {
                text: "About vibemix…"
                onTriggered: aboutWindow.show()
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
                        id: bpmReadout
                        visible: root.transportUi().bpm
                        text: root.clockInfo().bpm.toFixed(1) + " BPM"
                        color: root.clockInfo().locked ? root.textColor : root.mutedText
                        font.pixelSize: 11
                        font.family: "monospace"

                        HoverHandler { id: bpmHover }
                        ToolTip.visible: bpmHover.hovered
                        ToolTip.text: root.clockInfo().locked ? "host tempo (MIDI clock)" : "no MIDI clock"
                        ToolTip.delay: 300
                    }

                    Label {
                        id: stabilityReadout
                        visible: root.transportUi().stability && backend.clock.length > 0
                        text: root.stabilityText()
                        color: root.stabilityColor()
                        font.pixelSize: 11
                        font.family: "monospace"

                        HoverHandler { id: stabilityHover }
                        ToolTip.visible: stabilityHover.hovered
                        ToolTip.text: root.stabilityTooltip()
                        ToolTip.delay: 300
                    }

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

                    Rectangle {
                        id: recordDot
                        Layout.alignment: Qt.AlignVCenter
                        implicitWidth: 16
                        implicitHeight: 16
                        width: 16
                        height: 16
                        radius: 8
                        border.color: root.borderColor
                        border.width: 1
                        color: {
                            var r = root.recorderInfo()
                            if (r.state === "recording") return "#e53935"
                            if (r.state === "armed") return "#f9a825"
                            return root.controlColor
                        }

                        HoverHandler { id: recordHover }
                        ToolTip.visible: recordHover.hovered
                        ToolTip.delay: 300
                        ToolTip.text: {
                            var r = root.recorderInfo()
                            if (r.message) return r.message
                            if (r.state === "recording") return "recording " + Math.round((r.progress || 0) * 100) + "%"
                            if (r.state === "armed") return "armed: waiting for bar 1"
                            return "record a loop"
                        }

                        MouseArea {
                            anchors.fill: parent
                            onClicked: {
                                var r = root.recorderInfo()
                                root.sendRecorderAction({ action: r.state === "idle" ? "arm" : "cancel" })
                            }
                        }
                    }

                    Label {
                        visible: root.recorderInfo().state !== "idle"
                        text: {
                            var r = root.recorderInfo()
                            if (r.state === "recording") return "REC " + Math.round((r.progress || 0) * 100) + "%"
                            if (r.state === "armed") return "ARM"
                            return ""
                        }
                        color: root.recorderInfo().state === "recording" ? "#e53935" : "#f9a825"
                        font.pixelSize: 10
                        font.family: "monospace"
                    }

                    Row {
                        id: loopIndicator
                        visible: root.transportUi().loop_indicator && backend.clock.length > 0
                        spacing: 0

                        // Cells are loop units. In beats mode the first beat of
                        // every bar carries a wider gap on its left, so bars read
                        // as groups; in bars mode each cell is already a bar.
                        Repeater {
                            model: root.loopInfo().count

                            Item {
                                property int gap: root.transportUi().loop_bars
                                    ? 2
                                    : ((index > 0 && index % root.loopBeatsPerBar() === 0) ? 6 : 2)
                                width: 6 + (index === 0 ? 0 : gap)
                                height: 6

                                Rectangle {
                                    width: 6
                                    height: 6
                                    anchors.right: parent.right
                                    color: index <= root.loopInfo().index ? root.accentColor : root.borderColor
                                }
                            }
                        }
                    }

                    Label {
                        id: dspLabel
                        // Audio-callback load: average, with the recent peak in the tooltip.
                        text: "DSP " + Math.round(backend.dsp_load / 10) + "%"
                        visible: backend.connected
                        color: backend.dsp_peak >= 900 ? "#e53935"
                             : backend.dsp_load >= 600 ? "#fb8c00"
                             : root.mutedText
                        font.pixelSize: 11

                        HoverHandler { id: dspHover }
                        ToolTip.visible: dspHover.hovered
                        ToolTip.text: "audio callback load (average " + Math.round(backend.dsp_load / 10)
                                      + "%, recent peak " + Math.round(backend.dsp_peak / 10) + "%)"
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
                        text: "Tempo (BPM) readout"
                        checkable: true
                        checked: root.transportUi().bpm
                        onTriggered: root.setTransportUi({ bpm: !root.transportUi().bpm })
                    }
                    MenuItem {
                        text: "Clock stability readout"
                        checkable: true
                        checked: root.transportUi().stability
                        onTriggered: root.setTransportUi({ stability: !root.transportUi().stability })
                    }
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
                    MenuItem {
                        text: "Recorder…"
                        onTriggered: root.openRecorder()
                    }
                    MenuItem {
                        text: "Loops…"
                        onTriggered: loopsWindow.show()
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

    FolderDialog {
        id: loopFolderDialog
        title: "Loop folder"
        currentFolder: root.loopFolderUrl()
        onAccepted: root.updateRecorderSetting("save_dir", root.localPath(selectedFolder))
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
            // With auto-fit on, the size bindings own the window dimensions.
            if (root.transportUi().autosize) return
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
        pickerList.currentIndex = 0
        pickerWindow.show()
    }

    Connections {
        target: backend
        function onSnapshotChanged() { root.applySnapshot(JSON.parse(backend.snapshot)); root.pruneEditors() }
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

    // Escape hatch: abort a running (or hung) plugin sweep.
    Shortcut {
        sequence: "Escape"
        enabled: backend.scan_running
        onActivated: backend.cancelPluginScan()
    }

    // Keyboard bindings for mixer buttons are handled by the main content
    // item (see `handleGlobalKey`), so unhandled key events bubble up from the
    // focused control.

    // Waiting-for-host overlay: shown until the engine is connected and has sent
    // its plugin catalog, and while the startup plugin sweep is running.
    Rectangle {
        id: hostOverlay
        anchors.fill: parent
        color: Qt.rgba(0, 0, 0, 0.55)
        visible: !backend.connected || !backend.catalog_ready
        z: 1000

        ColumnLayout {
            anchors.centerIn: parent
            spacing: 10

            BusyIndicator {
                Layout.alignment: Qt.AlignHCenter
                running: hostOverlay.visible
                implicitWidth: 48
                implicitHeight: 48
            }

            Label {
                Layout.alignment: Qt.AlignHCenter
                text: backend.connected ? "loading session…" : "waiting for the engine…"
                color: "white"
                font.pixelSize: 13
            }
        }
    }

    // Thin sweep progress strip at the top while plugins are being tested.
    Rectangle {
        id: scanBar
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        height: 3
        visible: backend.scan_running
        color: root.panelColor
        z: 1001

        Rectangle {
            height: parent.height
            color: root.accentColor
            width: backend.scan_total > 0
                ? parent.width * backend.scan_done / backend.scan_total
                : 0
        }
    }

    Label {
        anchors.top: scanBar.bottom
        anchors.right: parent.right
        anchors.topMargin: 2
        anchors.rightMargin: 8
        visible: backend.scan_running
        text: backend.scan_total > 0
            ? "testing plugins " + backend.scan_done + "/" + backend.scan_total + " (esc to cancel)"
            : "testing plugins… (esc to cancel)"
        color: root.textColor
        font.pixelSize: 9
        z: 1001
    }

    // Compact checkbox used by the plugin browser filter rows.
    component FilterCheck: CheckBox {
        spacing: 4
        leftPadding: 4
        rightPadding: 4
        topPadding: 0
        bottomPadding: 0
        font.pixelSize: 10
    }

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
                : root.bindingLabel(cell.spec.kind, cell.spec.location, cell.spec.target,
                    cell.spec.slot, cell.spec.param)
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
                text: "Learn key"
                enabled: cell.spec !== null && root.isButtonKind(cell.spec.kind)
                onTriggered: root.beginKeyLearn(cell.spec)
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
            cursorShape: Qt.OpenHandCursor
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onSingleTapped: grip.menuRequested()
        }

        DragHandler {
            id: gripDrag
            target: null
            enabled: grip.reorderEnabled
            cursorShape: Qt.ClosedHandCursor
            onTranslationChanged: {
                grip.card.dragOffset = gripDrag.translation.x
                grip.card.dragOffsetY = gripDrag.translation.y
            }
            onActiveChanged: {
                if (!grip.card) return
                if (gripDrag.active) {
                    grip.card.dragging = true
                    root.gripDragging = true
                } else {
                    var index = root.dropIndexFor(
                        grip.row, grip.card, grip.card.dragOffset,
                        grip.card.dragOffsetY, grip.kind)
                    grip.card.dragging = false
                    grip.card.dragOffset = 0
                    grip.card.dragOffsetY = 0
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
            width: parent.width
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
                    ToolTip.text: pluginRow.pluginData.missing
                        ? pluginRow.pluginData.name + " (missing)"
                        : pluginRow.pluginData.name
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
                            }
                        }
                        onDoubleClicked: (mouse) => {
                            if (mouse.button === Qt.LeftButton) {
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
                        MenuItem {
                            text: "Sidechain"
                            visible: pluginRow.pluginData.has_sidechain === true
                            checkable: true
                            checked: pluginRow.pluginData.sidechain_enabled === true
                            onTriggered: backend.setPluginSidechainEnabled(
                                pluginListRoot.location,
                                pluginListRoot.target,
                                pluginRow.pluginData.slot,
                                !(pluginRow.pluginData.sidechain_enabled === true))
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
        property real liveValue: 1
        property real resetValue: 1
        property real dragStart: 0
        property string valueText: ""
        property string entryUnit: "db"
        readonly property bool interacting: dragHandler.active || valueTimer.running
        readonly property real displayValue: fader.interacting ? fader.liveValue : fader.value
        readonly property string shownValueText: fader.interacting
            ? root.gainText(fader.liveValue)
            : fader.valueText
        signal moved(real value)

        implicitWidth: 22
        implicitHeight: 150

        onValueChanged: if (!fader.interacting) fader.liveValue = fader.value
        onInteractingChanged: if (!fader.interacting) fader.liveValue = fader.value
        Component.onCompleted: fader.liveValue = fader.value

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

        readonly property real normalized: (to > from) ? Math.max(0, Math.min(1, (fader.displayValue - from) / (to - from))) : 0

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
            // Start moving as soon as the pointer moves: without this, Qt waits
            // for the platform drag distance (~10px), leaving a dead zone before
            // the value reacts.
            dragThreshold: 0
            grabPermissions: PointerHandler.TakeOverForbidden
            onActiveChanged: {
                if (dragHandler.active) {
                    fader.dragStart = fader.liveValue
                    fader.bump()
                }
            }
            onTranslationChanged: {
                var span = fader.to - fader.from
                var delta = -dragHandler.translation.y / track.height * span
                var next = Math.max(fader.from, Math.min(fader.to, fader.dragStart + delta))
                fader.liveValue = next
                fader.moved(next)
                fader.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.NoModifier
            onSingleTapped: (eventPoint) => {
                var y = eventPoint.position.y
                var clamped = Math.max(track.y, Math.min(track.y + track.height, y))
                var n = 1 - (clamped - track.y) / track.height
                var next = fader.from + n * (fader.to - fader.from)
                fader.liveValue = next
                fader.moved(next)
                fader.bump()
            }
            onDoubleTapped: {
                fader.liveValue = fader.resetValue
                fader.moved(fader.resetValue)
                fader.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                fader.liveValue = fader.resetValue
                fader.moved(fader.resetValue)
                fader.bump()
            }
        }

        WheelHandler {
            onWheel: (e) => {
                var step = (fader.to - fader.from) * 0.03 * (e.angleDelta.y > 0 ? 1 : -1)
                var next = Math.max(fader.from, Math.min(fader.to, fader.liveValue + step))
                fader.liveValue = next
                fader.moved(next)
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
                fader.liveValue = v
                fader.moved(v)
                fader.bump()
            }
        }

        Rectangle {
            z: 100
            visible: fader.interacting && fader.shownValueText.length > 0
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
                text: fader.shownValueText
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
        property bool preFader: false
        property var formatValue: function(v) { return Number(v).toFixed(2) }
        readonly property string valueText: slider.formatValue ? slider.formatValue(slider.displayValue) : ""
        readonly property bool interacting: slider.dragging || valueTimer.running
        readonly property real displayValue: slider.interacting ? slider.liveValue : slider.value
        signal moved(real value)
        signal delayMoved(real ms)
        signal preFaderToggled(bool pre)

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
                slider.liveValue = slider.resetValue
                slider.moved(slider.resetValue)
                slider.bump()
            }
            onWheel: (wheel) => {
                var step = wheel.angleDelta.y > 0 ? 0.03 : -0.03
                var next = slider.norm(slider.liveValue + step)
                slider.liveValue = next
                slider.moved(next)
                slider.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                slider.liveValue = slider.resetValue
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
            MenuItem {
                text: "Pre-fader"
                checkable: true
                checked: slider.preFader
                onTriggered: slider.preFaderToggled(!slider.preFader)
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
        property real liveValue: 0
        property real dragStart: 0
        property real resetValue: (from + to) / 2
        property string valueText: ""
        property string entryUnit: "linear"
        readonly property bool interacting: knobDrag.active || valueTimer.running
        readonly property real displayValue: knob.interacting ? knob.liveValue : knob.value
        readonly property string shownValueText: {
            if (!knob.interacting) return knob.valueText
            if (entryUnit === "pan") return root.panText(knob.liveValue)
            if (entryUnit === "db") return root.trimText(knob.liveValue)
            if (entryUnit === "gain-db") return root.gainText(knob.liveValue)
            return Number(knob.liveValue).toFixed(3)
        }
        signal moved(real value)

        implicitWidth: 26
        implicitHeight: 26

        onValueChanged: if (!knob.interacting) knob.liveValue = knob.value
        onInteractingChanged: if (!knob.interacting) knob.liveValue = knob.value
        Component.onCompleted: knob.liveValue = knob.value

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

        readonly property real normalized: (to > from) ? Math.max(0, Math.min(1, (knob.displayValue - from) / (to - from))) : 0

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
            dragThreshold: 0
            grabPermissions: PointerHandler.TakeOverForbidden
            onActiveChanged: {
                if (knobDrag.active) {
                    knob.dragStart = knob.liveValue
                    knob.bump()
                }
            }
            onTranslationChanged: {
                var delta = -knobDrag.translation.y / 120 * (knob.to - knob.from)
                var next = Math.max(knob.from, Math.min(knob.to, knob.dragStart + delta))
                knob.liveValue = next
                knob.moved(next)
                knob.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.NoModifier
            onDoubleTapped: {
                knob.liveValue = knob.resetValue
                knob.moved(knob.resetValue)
                knob.bump()
            }
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                knob.liveValue = knob.resetValue
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
                knob.liveValue = v
                knob.moved(v)
                knob.bump()
            }
        }

        Rectangle {
            z: 100
            visible: knob.interacting && knob.shownValueText.length > 0
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
                text: knob.shownValueText
                color: root.textColor
                font.pixelSize: 9
                font.family: "monospace"
            }
        }
    }

    // Horizontal slider whose fill grows out of a `reference` point (e.g. 0 dB
    // trim, centred pan, unity send/return), so the neutral value sits in the
    // middle of the track. Shares the Knob's value-entry/tooltip behaviour.
    component MiniSlider: Item {
        id: mini
        property real from: -1
        property real to: 1
        property real value: 0
        property real liveValue: 0
        property real reference: (from + to) / 2
        property real resetValue: reference
        property bool dragging: false
        property string valueText: ""
        property string entryUnit: "linear"
        readonly property bool interacting: mini.dragging || valueTimer.running
        readonly property real displayValue: mini.interacting ? mini.liveValue : mini.value
        readonly property string shownValueText: {
            if (!mini.interacting) return mini.valueText
            if (entryUnit === "pan") return root.panText(mini.liveValue)
            if (entryUnit === "db") return root.trimText(mini.liveValue)
            if (entryUnit === "gain-db") return root.gainText(mini.liveValue)
            return Number(mini.liveValue).toFixed(3)
        }
        signal moved(real value)

        implicitWidth: 60
        implicitHeight: 14

        onValueChanged: if (!mini.interacting) mini.liveValue = mini.value
        onInteractingChanged: if (!mini.interacting) mini.liveValue = mini.value
        Component.onCompleted: mini.liveValue = mini.value

        function bump() { valueTimer.restart() }
        function clamp01(v) { return Math.max(0, Math.min(1, isNaN(v) ? 0 : v)) }
        function entryLabel() {
            if (entryUnit === "pan") return "pan"
            if (entryUnit === "db") return "trim"
            if (entryUnit === "gain-db") return "gain"
            return "value"
        }
        function entryFrom() {
            if (entryUnit === "gain-db") return -60
            if (entryUnit === "pan") return -1
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
        function setFromX(px) {
            var span = mini.to - mini.from
            var next = Math.max(mini.from, Math.min(mini.to, mini.from + px / track.width * span))
            mini.liveValue = next
            mini.moved(next)
        }

        Timer { id: valueTimer; interval: 700 }

        readonly property real normalized: mini.clamp01((mini.displayValue - mini.from) / (mini.to - mini.from))
        readonly property real referenceNorm: mini.clamp01((mini.reference - mini.from) / (mini.to - mini.from))

        Rectangle {
            id: track
            anchors.fill: parent
            color: root.meterBack
            border.color: root.borderColor
            border.width: 1
        }

        // Reference tick (the neutral value).
        Rectangle {
            x: Math.round(mini.referenceNorm * (track.width - 1))
            width: 1
            height: track.height
            color: root.borderColor
        }

        // Fill from the reference to the current value.
        Rectangle {
            anchors.verticalCenter: track.verticalCenter
            height: track.height - 4
            color: root.accentColor
            x: Math.min(mini.referenceNorm, mini.normalized) * track.width + 1
            width: Math.abs(mini.normalized - mini.referenceNorm) * (track.width - 2)
        }

        Rectangle {
            id: thumb
            width: 6
            height: track.height
            x: mini.normalized * (track.width - thumb.width)
            color: mini.dragging ? Qt.lighter(root.controlColor, 1.2) : root.controlColor
            border.color: mini.dragging ? root.accentColor : root.borderColor
            border.width: 1
        }

        MouseArea {
            anchors.fill: parent
            acceptedButtons: Qt.LeftButton
            preventStealing: true
            onPressed: (mouse) => {
                mini.liveValue = mini.value
                mini.dragging = true
                mini.setFromX(mouse.x)
                mini.bump()
            }
            onPositionChanged: (mouse) => {
                if (mini.dragging) {
                    mini.setFromX(mouse.x)
                    mini.bump()
                }
            }
            onReleased: mini.dragging = false
            onCanceled: mini.dragging = false
            onDoubleClicked: {
                mini.liveValue = mini.resetValue
                mini.moved(mini.resetValue)
                mini.bump()
            }
        }

        TapHandler {
            acceptedButtons: Qt.RightButton
            onSingleTapped: miniMenu.popup()
        }

        TapHandler {
            acceptedModifiers: Qt.ControlModifier
            onSingleTapped: {
                mini.liveValue = mini.resetValue
                mini.moved(mini.resetValue)
                mini.bump()
            }
        }

        WheelHandler {
            onWheel: (e) => {
                var step = (mini.to - mini.from) * 0.03 * (e.angleDelta.y > 0 ? 1 : -1)
                var next = Math.max(mini.from, Math.min(mini.to, mini.liveValue + step))
                mini.liveValue = next
                mini.moved(next)
                mini.bump()
            }
        }

        Menu {
            id: miniMenu
            MenuItem {
                text: "Set value…"
                onTriggered: miniEntry.openFor(mini.value)
            }
            MenuItem {
                text: "Reset"
                onTriggered: {
                    mini.liveValue = mini.resetValue
                    mini.moved(mini.resetValue)
                    mini.bump()
                }
            }
        }

        NumberEntry {
            id: miniEntry
            label: mini.entryLabel()
            from: mini.entryFrom()
            to: mini.entryTo()
            format: mini.entryFormat
            parse: mini.entryParse
            onCommitted: (v) => {
                mini.liveValue = v
                mini.moved(v)
                mini.bump()
            }
        }

        Rectangle {
            z: 100
            visible: mini.interacting && mini.shownValueText.length > 0
            anchors.horizontalCenter: parent.horizontalCenter
            y: -height - 2
            width: miniValueLabel.implicitWidth + 8
            height: miniValueLabel.implicitHeight + 2
            color: root.panelColor
            border.color: root.accentColor
            border.width: 1

            Label {
                id: miniValueLabel
                anchors.centerIn: parent
                text: mini.shownValueText
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
        property real liveValue: 0
        property bool interacting: false
        spacing: 6

        readonly property real currentValue: interacting
            ? liveValue
            : root.paramValue(paramLocation, paramTarget, paramSlot, paramIndex)
        readonly property bool isEnum: root.isEnumControl(controlData)

        function beginInteraction(value) {
            liveValue = value
            interacting = true
            interactTimer.restart()
        }

        Timer {
            id: interactTimer
            interval: 600
            onTriggered: parameterRow.interacting = false
        }

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
            onClicked: {
                parameterRow.beginInteraction(checked ? 1 : 0)
                backend.setPluginParam(
                    parameterRow.paramLocation,
                    parameterRow.paramTarget,
                    parameterRow.paramSlot,
                    parameterRow.paramIndex,
                    checked ? 1 : 0)
            }
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
            onActivated: {
                var chosen = parameterRow.controlData.scale_points[currentIndex].value
                parameterRow.beginInteraction(chosen)
                backend.setPluginParam(
                    parameterRow.paramLocation,
                    parameterRow.paramTarget,
                    parameterRow.paramSlot,
                    parameterRow.paramIndex,
                    chosen)
            }
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
            onMoved: {
                parameterRow.beginInteraction(value)
                backend.setPluginParam(
                    parameterRow.paramLocation,
                    parameterRow.paramTarget,
                    parameterRow.paramSlot,
                    parameterRow.paramIndex,
                    value)
            }
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
                            font.pixelSize: 11
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
        // Browser filters — "show" toggles default on; `scOnly` is a filter that
        // restricts to sidechain-capable plugins.
        property bool showLv2: true
        property bool showVst3: true
        property bool showClap: true
        property bool showMono: true
        property bool showStereo: true
        property bool showFailed: true
        property bool scOnly: false
        // Model and selection are shared by the list and the details panel.
        property var filtered: root.filteredCatalog(pickerFilter.text, pickerWindow)
        property var currentEntry: (pickerList.currentIndex >= 0 && pickerList.currentIndex < filtered.length)
            ? filtered[pickerList.currentIndex] : null

        modality: Qt.ApplicationModal
        flags: Qt.Dialog
        transientParent: root
        visible: false
        width: 720
        height: 540
        minimumWidth: 560
        minimumHeight: 260
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
                    text: backend.scan_running
                        ? "testing " + backend.scan_done + "/" + backend.scan_total
                        : "test all"
                    enabled: backend.connected && !backend.scan_running
                    onClicked: backend.retestPlugins()
                }

                FlatButton {
                    visible: backend.scan_running
                    text: "cancel"
                    onClicked: backend.cancelPluginScan()
                }

                FlatButton {
                    text: "close"
                    onClicked: pickerWindow.close()
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 8

                TextField {
                    id: pickerFilter
                    Layout.fillWidth: true
                    implicitHeight: 24
                    font.pixelSize: 11
                    placeholderText: "filter..."
                }

                FilterCheck {
                    text: "LV2"
                    checked: pickerWindow.showLv2
                    onToggled: pickerWindow.showLv2 = checked
                }
                FilterCheck {
                    text: "VST3"
                    checked: pickerWindow.showVst3
                    onToggled: pickerWindow.showVst3 = checked
                }
                FilterCheck {
                    text: "CLAP"
                    checked: pickerWindow.showClap
                    onToggled: pickerWindow.showClap = checked
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 12

                FilterCheck {
                    text: "Mono"
                    checked: pickerWindow.showMono
                    onToggled: pickerWindow.showMono = checked
                }
                FilterCheck {
                    text: "Stereo"
                    checked: pickerWindow.showStereo
                    onToggled: pickerWindow.showStereo = checked
                }
                FilterCheck {
                    text: "SC only"
                    checked: pickerWindow.scOnly
                    onToggled: pickerWindow.scOnly = checked
                    ToolTip.visible: hovered
                    ToolTip.text: "Show only sidechain-capable plugins"
                }
                Item { Layout.fillWidth: true }
                FilterCheck {
                    text: "Failed"
                    checked: pickerWindow.showFailed
                    onToggled: pickerWindow.showFailed = checked
                    ToolTip.visible: hovered
                    ToolTip.text: "Show plugins that failed the smoke test"
                }
            }

            SplitView {
                Layout.fillWidth: true
                Layout.fillHeight: true
                orientation: Qt.Horizontal

                ColumnLayout {
                    SplitView.minimumWidth: 260
                    SplitView.preferredWidth: pickerWindow.width * 0.56
                    spacing: 4

                    Label {
                        Layout.fillWidth: true
                        text: pickerWindow.filtered.length + " / " + catalog.length + " plugins"
                        color: root.mutedText
                        font.pixelSize: 9
                    }

                    ListView {
                        id: pickerList
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        model: pickerWindow.filtered
                        boundsBehavior: Flickable.StopAtBounds
                        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

                        delegate: Rectangle {
                            id: pickerRow
                            width: ListView.view.width
                            height: 34
                            color: pickerMouse.containsMouse || ListView.isCurrentItem
                                ? root.controlColor : "transparent"

                            Column {
                                anchors.left: parent.left
                                anchors.leftMargin: 6
                                anchors.right: rightInfo.left
                                anchors.rightMargin: 6
                                anchors.verticalCenter: parent.verticalCenter
                                spacing: 1

                                Label {
                                    width: parent.width
                                    text: modelData.name
                                    elide: Text.ElideRight
                                    color: root.pluginFailed(modelData) ? "#e53935" : root.textColor
                                    font.pixelSize: 11
                                }
                                Label {
                                    width: parent.width
                                    text: root.pluginSubtitle(modelData)
                                    elide: Text.ElideRight
                                    color: root.mutedText
                                    font.pixelSize: 9
                                    visible: text.length > 0
                                }
                            }

                            Row {
                                id: rightInfo
                                anchors.right: parent.right
                                anchors.rightMargin: 8
                                anchors.verticalCenter: parent.verticalCenter
                                spacing: 6

                                Label {
                                    text: "SC"
                                    visible: modelData.has_sidechain === true
                                    color: root.mutedText
                                    font.pixelSize: 8
                                    font.bold: true
                                }
                                Label {
                                    text: root.formatLabel(modelData)
                                    color: root.mutedText
                                    font.pixelSize: 8
                                }
                                Label {
                                    id: statusLabel
                                    text: root.pluginStatusText(modelData)
                                    color: "#e53935"
                                    font.pixelSize: 9
                                    visible: text.length > 0
                                }
                            }

                            MouseArea {
                                id: pickerMouse
                                anchors.fill: parent
                                hoverEnabled: true
                                onEntered: pickerList.currentIndex = index
                                onClicked: {
                                    backend.addPlugin(pickerWindow.location, pickerWindow.target, modelData.uri)
                                    pickerWindow.close()
                                }
                            }
                        }
                    }
                }

                ScrollView {
                    id: pickerDetails
                    SplitView.minimumWidth: 220
                    SplitView.fillWidth: true
                    contentWidth: availableWidth
                    clip: true

                    ColumnLayout {
                        width: pickerDetails.availableWidth
                        spacing: 5

                        Label {
                            Layout.fillWidth: true
                            text: pickerWindow.currentEntry ? pickerWindow.currentEntry.name : "Select a plugin"
                            color: root.textColor
                            font.bold: true
                            font.pixelSize: 12
                            wrapMode: Text.WordWrap
                        }

                        Label {
                            Layout.fillWidth: true
                            visible: text.length > 0
                            text: pickerWindow.currentEntry ? pickerWindow.currentEntry.description : ""
                            color: root.mutedText
                            font.pixelSize: 10
                            wrapMode: Text.WordWrap
                        }

                        Repeater {
                            model: root.pluginDetails(pickerWindow.currentEntry)
                            delegate: RowLayout {
                                Layout.fillWidth: true
                                spacing: 6

                                Label {
                                    text: modelData.label
                                    color: root.mutedText
                                    font.pixelSize: 9
                                    Layout.preferredWidth: 60
                                }
                                Label {
                                    Layout.fillWidth: true
                                    text: modelData.value
                                    color: root.textColor
                                    font.pixelSize: 9
                                    elide: Text.ElideMiddle
                                }
                            }
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
            // Grab keys while capturing a keyboard binding.
            focus: root.midiKeyLearnSpec !== null
            Keys.onPressed: (event) => root.handleKeyCapture(event)

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

            Label {
                Layout.fillWidth: true
                visible: root.midiKeyLearnSpec !== null
                text: "press a key… (Esc cancels, Backspace clears)"
                color: root.accentColor
                font.pixelSize: 11
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
                    font.pixelSize: 11
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

    Window {
        id: aboutWindow
        title: "About vibemix"
        modality: Qt.ApplicationModal
        flags: Qt.Dialog
        transientParent: root
        visible: false
        width: 420
        height: 330
        color: palette.window

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 12
            spacing: 8

            Label {
                text: "vibemix " + Qt.application.version
                color: root.textColor
                font.pixelSize: 16
                font.bold: true
            }

            Label {
                Layout.fillWidth: true
                text: "A modular LV2/VST3/CLAP audio mixer for JACK/PipeWire."
                color: root.textColor
                font.pixelSize: 11
                wrapMode: Text.WordWrap
            }

            Label {
                Layout.fillWidth: true
                text: "A vibe-coding experiment: built by an AI assistant with human direction — expect rough edges."
                color: root.mutedText
                font.pixelSize: 11
                wrapMode: Text.WordWrap
            }

            Label {
                Layout.fillWidth: true
                text: "License: MIT OR Apache-2.0"
                color: root.textColor
                font.pixelSize: 11
            }

            Label {
                Layout.fillWidth: true
                textFormat: Text.RichText
                text: "<a href=\"https://github.com/Llamatron2112/vibemix\">https://github.com/Llamatron2112/vibemix</a>"
                onLinkActivated: (link) => Qt.openUrlExternally(link)
                font.pixelSize: 11
                wrapMode: Text.WordWrap
            }

            Label {
                text: "Built with"
                color: root.mutedText
                font.pixelSize: 10
                font.bold: true
            }

            Label {
                Layout.fillWidth: true
                text: "Qt 6 / Qt Quick (and cxx-qt)\nLV2 (lilv)\nSteinberg VST3 SDK\nCLAP\nJACK / PipeWire"
                color: root.textColor
                font.pixelSize: 11
            }

            Item { Layout.fillHeight: true }

            RowLayout {
                Layout.fillWidth: true
                Item { Layout.fillWidth: true }
                FlatButton {
                    text: "close"
                    onClicked: aboutWindow.close()
                }
            }
        }
    }

    Window {
        id: patchWindow
        title: "Patchbay"
        modality: Qt.ApplicationModal
        flags: Qt.Dialog
        transientParent: root
        visible: false
        width: 940
        height: 620
        color: palette.window

        property string sourceClient: ""
        property string sinkClient: ""
        property bool onlyConnected: false
        property string hoverSource: ""
        property string hoverSink: ""
        readonly property var sources: root.patchSources(sourceClient)
        readonly property var sinks: root.patchSinks(sinkClient)
        readonly property int labelWidth: 190
        readonly property int cellSize: 22
        readonly property int headerHeight: 86

        function refreshClients() {
            root.ensurePatchClients()
            backend.queryPorts()
        }

        onVisibleChanged: if (visible) refreshClients()

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 6

            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Label { text: "Source device"; color: root.mutedText; font.pixelSize: 11 }
                ComboBox {
                    Layout.preferredWidth: 150
                    font.pixelSize: 11
                    model: root.portClients()
                    currentIndex: Math.max(0, root.portClients().indexOf(patchWindow.sourceClient))
                    onActivated: (index) => patchWindow.sourceClient = root.portClients()[index]
                }
                Label { text: "Sink device"; color: root.mutedText; font.pixelSize: 11 }
                ComboBox {
                    Layout.preferredWidth: 150
                    font.pixelSize: 11
                    model: root.portClients()
                    currentIndex: Math.max(0, root.portClients().indexOf(patchWindow.sinkClient))
                    onActivated: (index) => patchWindow.sinkClient = root.portClients()[index]
                }
                Item { Layout.fillWidth: true }
                FilterCheck {
                    text: "Only connected"
                    checked: patchWindow.onlyConnected
                    onToggled: patchWindow.onlyConnected = checked
                }
                FlatButton {
                    text: "refresh"
                    onClicked: backend.queryPorts()
                }
                FlatButton {
                    text: "close"
                    onClicked: patchWindow.close()
                }
            }

            Label {
                Layout.fillWidth: true
                text: backend.connection_error.length > 0
                    ? backend.connection_error
                    : root.portList().length + " ports from " + root.portClients().length
                      + " devices. Rows are sources (outputs), columns are sinks (inputs); only vibemix connections are editable."
                color: backend.connection_error.length > 0 ? "#e53935" : root.mutedText
                font.pixelSize: 10
                elide: Text.ElideRight
            }

            Flickable {
                Layout.fillWidth: true
                Layout.fillHeight: true
                clip: true
                contentWidth: matrix.implicitWidth
                contentHeight: matrix.implicitHeight
                ScrollBar.vertical: ScrollBar { }
                ScrollBar.horizontal: ScrollBar { }

                Column {
                    id: matrix
                    spacing: 2

                    Row {
                        spacing: 2
                        Item { width: patchWindow.labelWidth; height: patchWindow.headerHeight }
                        Repeater {
                            model: patchWindow.sinks
                            Item {
                                width: patchWindow.cellSize
                                height: patchWindow.headerHeight
                                Label {
                                    width: parent.height - 8
                                    text: root.portShortLabel(modelData)
                                    color: patchWindow.hoverSink === modelData.name
                                        ? root.accentColor : root.mutedText
                                    font.pixelSize: 8
                                    font.bold: patchWindow.hoverSink === modelData.name
                                    elide: Text.ElideRight
                                    horizontalAlignment: Text.AlignRight
                                    rotation: -90
                                    anchors.centerIn: parent
                                }
                            }
                        }
                    }

                    Row {
                        spacing: 2
                        Item { width: patchWindow.labelWidth; height: 14 }
                        Repeater {
                            model: patchWindow.sinks
                            Item {
                                width: patchWindow.cellSize
                                height: 14
                                Rectangle {
                                    visible: modelData.role !== undefined && modelData.role !== null
                                    width: 10
                                    height: 10
                                    anchors.centerIn: parent
                                    property real level: root.portLevel(modelData)
                                    property real frac: root.levelFraction(level)
                                    color: frac > 0.01 ? root.levelColor(level) : root.controlColor
                                    border.color: root.borderColor
                                    border.width: 1
                                    opacity: 0.35 + 0.65 * frac
                                }
                            }
                        }
                    }

                    Repeater {
                        model: patchWindow.sources
                        Row {
                            id: sourceRow
                            spacing: 2
                            property var sourceData: patchWindow.sources[index]
                            Row {
                                width: patchWindow.labelWidth
                                height: patchWindow.cellSize
                                spacing: 4
                                Label {
                                    width: parent.width - 14
                                    height: parent.height
                                    text: root.portShortLabel(sourceRow.sourceData)
                                    color: patchWindow.hoverSource === sourceRow.sourceData.name
                                        ? root.accentColor : root.textColor
                                    font.pixelSize: 10
                                    font.bold: patchWindow.hoverSource === sourceRow.sourceData.name
                                    verticalAlignment: Text.AlignVCenter
                                    elide: Text.ElideRight
                                }
                                Rectangle {
                                    visible: sourceRow.sourceData.role !== undefined
                                        && sourceRow.sourceData.role !== null
                                    width: 10
                                    height: 10
                                    anchors.verticalCenter: parent.verticalCenter
                                    property real level: root.portLevel(sourceRow.sourceData)
                                    property real frac: root.levelFraction(level)
                                    color: frac > 0.01 ? root.levelColor(level) : root.controlColor
                                    border.color: root.borderColor
                                    border.width: 1
                                    opacity: 0.35 + 0.65 * frac
                                }
                            }
                            Repeater {
                                model: patchWindow.sinks
                                Rectangle {
                                    id: cell
                                    property var sinkData: patchWindow.sinks[index]
                                    property bool canConnect: root.patchCanConnect(sourceRow.sourceData, cell.sinkData)
                                    property bool isOn: root.portConnected(sourceRow.sourceData.name, cell.sinkData)
                                    property bool hovered: patchWindow.hoverSource === sourceRow.sourceData.name
                                        || patchWindow.hoverSink === cell.sinkData.name
                                    width: patchWindow.cellSize
                                    height: patchWindow.cellSize
                                    color: cell.isOn ? root.accentColor : "transparent"
                                    border.color: cell.hovered ? root.accentColor : root.borderColor
                                    border.width: 1
                                    opacity: cell.canConnect ? 1.0 : 0.2
                                    MouseArea {
                                        anchors.fill: parent
                                        hoverEnabled: true
                                        onClicked: {
                                            if (cell.canConnect) {
                                                root.togglePortCell(
                                                    sourceRow.sourceData, cell.sinkData, !cell.isOn)
                                            }
                                        }
                                        onEntered: {
                                            patchWindow.hoverSource = sourceRow.sourceData.name
                                            patchWindow.hoverSink = cell.sinkData.name
                                        }
                                        onExited: {
                                            if (patchWindow.hoverSource === sourceRow.sourceData.name
                                                && patchWindow.hoverSink === cell.sinkData.name) {
                                                patchWindow.hoverSource = ""
                                                patchWindow.hoverSink = ""
                                            }
                                        }
                                        ToolTip.visible: containsMouse
                                        ToolTip.delay: 400
                                        ToolTip.text: root.portShortLabel(sourceRow.sourceData)
                                            + " → " + root.portShortLabel(cell.sinkData)
                                            + (cell.isOn ? "  (connected)" : "")
                                    }
                                }
                            }
                        }
                    }

                    Label {
                        visible: patchWindow.sources.length === 0 || patchWindow.sinks.length === 0
                        text: "No ports to show for this device selection."
                        color: root.mutedText
                        font.pixelSize: 11
                    }
                }
            }
        }

        Timer {
            id: patchRefreshTimer
            interval: 300
            repeat: false
            onTriggered: backend.queryPorts()
        }
        Timer {
            interval: 1500
            running: patchWindow.visible
            repeat: true
            onTriggered: backend.queryPorts()
        }
    }

    Window {
        id: midiWindow
        title: "MIDI input"
        modality: Qt.ApplicationModal
        flags: Qt.Dialog
        transientParent: root
        visible: false
        width: 480
        height: 420
        color: palette.window

        onVisibleChanged: if (visible) backend.queryPorts()

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 6

            Label {
                Layout.fillWidth: true
                text: root.midiInputPort()
                    ? "Ports connected to " + root.midiInputPort().name
                    : "No MIDI input port (is the engine connected?)"
                color: root.mutedText
                font.pixelSize: 10
                wrapMode: Text.WordWrap
            }

            Flickable {
                Layout.fillWidth: true
                Layout.fillHeight: true
                clip: true
                contentWidth: midiList.width
                contentHeight: midiList.implicitHeight
                ScrollBar.vertical: ScrollBar { }

                Column {
                    id: midiList
                    width: midiWindow.width - 24
                    spacing: 2

                    Repeater {
                        model: root.midiSources()
                        FilterCheck {
                            width: midiList.width
                            text: modelData.client + ": " + modelData.short
                            checked: root.midiConnected(modelData)
                            onToggled: root.toggleMidiSource(modelData, checked)
                        }
                    }

                    Label {
                        visible: root.midiSources().length === 0
                        text: "No MIDI output ports found."
                        color: root.mutedText
                        font.pixelSize: 11
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Item { Layout.fillWidth: true }
                FlatButton {
                    text: "refresh"
                    onClicked: backend.queryPorts()
                }
                FlatButton {
                    text: "close"
                    onClicked: midiWindow.close()
                }
            }
        }

        Timer {
            id: midiRefreshTimer
            interval: 300
            repeat: false
            onTriggered: backend.queryPorts()
        }
        Timer {
            interval: 1500
            running: midiWindow.visible
            repeat: true
            onTriggered: backend.queryPorts()
        }
    }

    Dialog {
        id: layoutDialog
        title: "Mixer layout"
        modal: true
        anchors.centerIn: parent
        padding: 10
        standardButtons: Dialog.Ok | Dialog.Cancel

        onOpened: {
            stripsPerRowBox.currentIndex = root.layoutColumns("strip")
            busesPerRowBox.currentIndex = root.layoutColumns("bus")
        }
        onAccepted: backend.setLayout(
            stripsPerRowBox.currentIndex, busesPerRowBox.currentIndex)

        contentItem: ColumnLayout {
            width: layoutDialog.availableWidth
            spacing: 8

            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Label {
                    text: "Strips per row"
                    color: root.textColor
                    font.pixelSize: 11
                    Layout.fillWidth: true
                }
                ComboBox {
                    id: stripsPerRowBox
                    Layout.preferredWidth: 100
                    font.pixelSize: 11
                    model: ["Auto", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12"]
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Label {
                    text: "Buses per row"
                    color: root.textColor
                    font.pixelSize: 11
                    Layout.fillWidth: true
                }
                ComboBox {
                    id: busesPerRowBox
                    Layout.preferredWidth: 100
                    font.pixelSize: 11
                    model: ["Auto", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12"]
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

    SplitView {
        id: mixerPanes
        anchors.fill: parent
        orientation: Qt.Horizontal
        focus: true
        Keys.onPressed: (event) => root.handleGlobalKey(event)

        // ---- Strips ----
        Item {
            SplitView.fillWidth: true
            SplitView.minimumWidth: root.transportUi().autosize ? 120 : 240

            ColumnLayout {
                anchors.fill: parent
                spacing: 0

                Rectangle {
                    Layout.fillWidth: true
                    implicitHeight: 24
                    color: palette.button

                    RowLayout {
                        anchors.fill: parent
                        anchors.leftMargin: 8
                        anchors.rightMargin: 4
                        spacing: 6

                        Label {
                            text: "Strips"
                            color: root.textColor
                            font.pixelSize: 11
                            font.bold: true
                        }
                        Label {
                            text: mix.strips.length
                                + (mix.strip_capacity !== undefined ? " / " + mix.strip_capacity : "")
                            color: root.mutedText
                            font.pixelSize: 10
                        }
                        Item { Layout.fillWidth: true }
                        FlatButton {
                            text: "add"
                            enabled: backend.connected && root.canAddStrip()
                            onClicked: backend.addStrip()
                        }
                    }
                }

                Flickable {
                    id: stripsFlick
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    contentWidth: stripsGrid.implicitWidth + 8
                    contentHeight: stripsGrid.implicitHeight + 8
                    clip: true
                    interactive: !root.sliderHovered && !root.gripDragging
                    ScrollBar.vertical: ScrollBar { }
                    ScrollBar.horizontal: ScrollBar { policy: ScrollBar.AsNeeded }

                    Grid {
                        id: stripsGrid
                        x: 4
                        y: 4
                        columns: root.paneColumns("strip")
                        columnSpacing: 4
                        rowSpacing: 4

                        Repeater {
                            id: stripsRepeater
                            model: stripIdModel

                            delegate: Rectangle {
                        id: stripCard
                        property var stripData: root.stripAt(model.sid)
                        property string reorderKind: "strip"
                        property bool dragging: false
                        property real dragOffset: 0
                        property real dragOffsetY: 0
                        width: root.stripCardWidth
                        height: stripContent.implicitHeight + 12
                        color: root.panelColor
                        border.color: stripCard.dragging ? root.accentColor : root.borderColor
                        border.width: 1
                        z: stripCard.dragging ? 5 : 0
                        opacity: stripCard.dragging ? 0.85 : 1
                        transform: Translate { x: stripCard.dragOffset; y: stripCard.dragOffsetY }

                        // All strip cards share the same layout, so the first
                        // one defines the height used by the bus cards too.
                        Binding {
                            target: root
                            property: "mixerCardHeight"
                            value: stripCard.height
                            when: index === 0
                            restoreMode: Binding.RestoreNone
                        }

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
                                row: stripsGrid
                                kind: "strip"
                                onDropped: (index) => backend.moveStrip(stripCard.stripData.id, index)
                                onMenuRequested: stripMenu.popup()
                            }

                            NameEdit {
                                text: stripCard.stripData.name
                                Layout.preferredWidth: root.stripBody
                                onCommitted: (value) => backend.setStripName(stripCard.stripData.id, value)
                            }

                            Label {
                                text: "trim"
                                color: root.mutedText
                                font.pixelSize: 8
                                Layout.alignment: Qt.AlignLeft
                            }

                            MiniSlider {
                                Layout.fillWidth: true
                                from: -24
                                to: 24
                                resetValue: 0
                                entryUnit: "db"
                                value: root.stripInputGain(stripCard.stripData.id)
                                valueText: root.trimText(root.stripInputGain(stripCard.stripData.id))
                                onMoved: (value) => backend.setStripInputGain(stripCard.stripData.id, value)
                            }

                            PluginList {
                                location: 0
                                target: stripCard.stripData.id
                                plugins: stripCard.stripData.plugins
                                title: "pre"
                                Layout.preferredWidth: root.stripBody
                            }

                            RowLayout {
                                Layout.alignment: Qt.AlignHCenter
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

                                    FlatButton {
                                        text: root.meterSourceLabel(stripCard.stripData.id)
                                        implicitHeight: 18
                                        implicitWidth: 40
                                        Layout.alignment: Qt.AlignHCenter
                                        onClicked: root.cycleMeterSource(stripCard.stripData.id)
                                    }
                                }
                            }

                            Label {
                                text: "pan"
                                color: root.mutedText
                                font.pixelSize: 8
                                Layout.alignment: Qt.AlignLeft
                            }

                            MiniSlider {
                                Layout.fillWidth: true
                                from: -1
                                to: 1
                                resetValue: 0
                                entryUnit: "pan"
                                value: root.stripPan(stripCard.stripData.id)
                                valueText: root.panText(root.stripPan(stripCard.stripData.id))
                                onMoved: (value) => backend.setStripPan(stripCard.stripData.id, value)
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
                                    model: busIdModel

                                    delegate: Column {
                                        id: sendRow
                                        property var busData: root.busAt(model.bid)
                                        width: parent.width
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
                                            preFader: root.sendPreFader(stripCard.stripData.id, sendRow.busData.id)
                                            formatValue: root.gainText
                                            onMoved: (v) => backend.setSendGain(
                                                stripCard.stripData.id,
                                                sendRow.busData.id,
                                                v)
                                            onDelayMoved: (ms) => backend.setSendDelay(
                                                stripCard.stripData.id,
                                                sendRow.busData.id,
                                                ms)
                                            onPreFaderToggled: (pre) => backend.setSendPreFader(
                                                stripCard.stripData.id,
                                                sendRow.busData.id,
                                                pre)
                                        }
                                    }
                                }

                                Repeater {
                                    model: root.sidechainDestinations()

                                    delegate: Column {
                                        id: sidechainRow
                                        property var dest: modelData
                                        readonly property bool selfHost:
                                            sidechainRow.dest.hostStripId === stripCard.stripData.id
                                        width: parent.width
                                        spacing: 1

                                        RowLayout {
                                            width: parent.width
                                            spacing: 2

                                            Label {
                                                text: "sc"
                                                color: root.mutedText
                                                font.pixelSize: 8
                                            }

                                            Label {
                                                id: sidechainLabel
                                                Layout.fillWidth: true
                                                text: sidechainRow.dest.name
                                                    + " (" + sidechainRow.dest.hostName + ")"
                                                elide: Text.ElideRight
                                                color: sidechainRow.selfHost
                                                    ? root.mutedText : root.textColor
                                                font.pixelSize: 8

                                                HoverHandler { id: sidechainHover }
                                                ToolTip.visible: sidechainHover.hovered
                                                ToolTip.text: sidechainLabel.text
                                                ToolTip.delay: 350
                                            }
                                        }

                                        SendSlider {
                                            width: parent.width
                                            enabled: !sidechainRow.selfHost
                                            value: root.sidechainGain(
                                                sidechainRow.dest.location,
                                                sidechainRow.dest.target,
                                                sidechainRow.dest.slot,
                                                stripCard.stripData.id)
                                            preFader: root.sidechainPreFader(
                                                sidechainRow.dest.location,
                                                sidechainRow.dest.target,
                                                sidechainRow.dest.slot,
                                                stripCard.stripData.id)
                                            formatValue: root.gainText
                                            onMoved: (v) => backend.setSidechainSend(
                                                sidechainRow.dest.location,
                                                sidechainRow.dest.target,
                                                sidechainRow.dest.slot,
                                                stripCard.stripData.id,
                                                v)
                                            onPreFaderToggled: (pre) => backend.setSidechainPreFader(
                                                sidechainRow.dest.location,
                                                sidechainRow.dest.target,
                                                sidechainRow.dest.slot,
                                                stripCard.stripData.id,
                                                pre)
                                        }
                                    }
                                }
                            }

                            Row {
                                spacing: 3
                                Layout.alignment: Qt.AlignHCenter
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
                    }
                }
            }
        }

        // ---- Buses ----
        Item {
            SplitView.preferredWidth: root.transportUi().autosize
                ? Math.max(130, busesGrid.implicitWidth + 12)
                : Math.max(240, root.busCardWidth * 2 + 40)
            SplitView.minimumWidth: root.transportUi().autosize ? 120 : 200

            ColumnLayout {
                anchors.fill: parent
                spacing: 0

                Rectangle {
                    Layout.fillWidth: true
                    implicitHeight: 24
                    color: palette.button

                    RowLayout {
                        anchors.fill: parent
                        anchors.leftMargin: 8
                        anchors.rightMargin: 4
                        spacing: 6

                        Label {
                            text: "Buses"
                            color: root.textColor
                            font.pixelSize: 11
                            font.bold: true
                        }
                        Label {
                            text: mix.buses.length
                                + (mix.bus_capacity !== undefined ? " / " + mix.bus_capacity : "")
                            color: root.mutedText
                            font.pixelSize: 10
                        }
                        Item { Layout.fillWidth: true }
                        FlatButton {
                            text: "add"
                            enabled: backend.connected && root.canAddBus()
                            onClicked: backend.addBus()
                        }
                    }
                }

                Flickable {
                    id: busesFlick
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    contentWidth: busesGrid.implicitWidth + 8
                    contentHeight: busesGrid.implicitHeight + 8
                    clip: true
                    interactive: !root.sliderHovered && !root.gripDragging
                    ScrollBar.vertical: ScrollBar { }
                    ScrollBar.horizontal: ScrollBar { policy: ScrollBar.AsNeeded }

                    Grid {
                        id: busesGrid
                        x: 4
                        y: 4
                        columns: root.paneColumns("bus")
                        columnSpacing: 4
                        rowSpacing: 4

                        Repeater {
                            id: busesRepeater
                            model: busIdModel

                            delegate: Rectangle {
                        id: busCard
                        property var busData: root.busAt(model.bid)
                        property string reorderKind: "bus"
                        readonly property bool reorderable: busCard.busData.id !== 0
                        property bool dragging: false
                        property real dragOffset: 0
                        property real dragOffsetY: 0
                        width: root.busCardWidth
                        height: mix.strips.length > 0
                            ? Math.max(root.mixerCardHeight, busContent.implicitHeight + 12)
                            : busContent.implicitHeight + 12
                        color: root.panelColor
                        border.color: busCard.dragging ? root.accentColor : root.borderColor
                        border.width: 1
                        z: busCard.dragging ? 5 : 0
                        opacity: busCard.dragging ? 0.85 : 1
                        transform: Translate { x: busCard.dragOffset; y: busCard.dragOffsetY }

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
                                visible: busCard.busData.id !== 0
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
                                row: busesGrid
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
                                Layout.alignment: Qt.AlignHCenter
                            }

                            ColumnLayout {
                                Layout.fillWidth: true
                                visible: busCard.busData.return_inputs === true
                                spacing: 2

                                Label {
                                    text: "SEND"
                                    color: root.mutedText
                                    font.pixelSize: 8
                                    Layout.alignment: Qt.AlignLeft
                                }

                                MiniSlider {
                                    Layout.fillWidth: true
                                    from: 0
                                    to: 2
                                    resetValue: 1
                                    entryUnit: "gain-db"
                                    value: root.busSendGain(busCard.busData.id)
                                    valueText: root.gainText(root.busSendGain(busCard.busData.id))
                                    onMoved: (value) => backend.setBusSendGain(busCard.busData.id, value)
                                }

                                Label {
                                    text: "RET"
                                    color: root.mutedText
                                    font.pixelSize: 8
                                    Layout.alignment: Qt.AlignLeft
                                }

                                MiniSlider {
                                    Layout.fillWidth: true
                                    from: 0
                                    to: 2
                                    resetValue: 1
                                    entryUnit: "gain-db"
                                    value: root.busReturnGain(busCard.busData.id)
                                    valueText: root.gainText(root.busReturnGain(busCard.busData.id))
                                    onMoved: (value) => backend.setBusReturnGain(busCard.busData.id, value)
                                }
                            }

                            RowLayout {
                                Layout.alignment: Qt.AlignHCenter
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
            }
        }
    }

    Dialog {
        id: recorderDialog
        title: "recorder"
        modal: true
        anchors.centerIn: parent
        width: 480
        padding: 10

        contentItem: ColumnLayout {
            width: recorderDialog.availableWidth
            spacing: 8

            RowLayout {
                Layout.fillWidth: true
                Label {
                    text: "length"
                    color: root.textColor
                    font.pixelSize: 11
                }
                Label {
                    Layout.fillWidth: true
                    text: root.transportUi().loop_length
                        + (root.transportUi().loop_bars ? " bars" : " beats")
                    color: root.mutedText
                    font.pixelSize: 11
                }
                FlatButton {
                    text: "loop settings…"
                    onClicked: {
                        recorderDialog.close()
                        loopDialog.open()
                    }
                }
            }

            Label {
                text: "sources (summed)"
                color: root.textColor
                font.pixelSize: 11
            }

            Flickable {
                Layout.fillWidth: true
                Layout.preferredHeight: Math.min(220, recorderRowsColumn.implicitHeight)
                clip: true
                contentHeight: recorderRowsColumn.implicitHeight
                ScrollBar.vertical: ScrollBar { }

                ColumnLayout {
                    id: recorderRowsColumn
                    width: parent.width
                    spacing: 1

                    Repeater {
                        model: root.recorderRows

                        delegate: RowLayout {
                            Layout.fillWidth: true
                            spacing: 6
                            property int rowIndex: index

                            CheckBox {
                                checked: modelData.checked
                                onToggled: root.setRecorderRow(rowIndex, "checked", checked)
                            }
                            Label {
                                Layout.fillWidth: true
                                text: modelData.name
                                color: root.textColor
                                font.pixelSize: 11
                                elide: Text.ElideRight
                            }
                            ComboBox {
                                Layout.preferredWidth: 110
                                font.pixelSize: 11
                                model: modelData.taps
                                currentIndex: Math.max(0, modelData.taps.indexOf(modelData.tap))
                                enabled: modelData.checked
                                onActivated: root.setRecorderRow(rowIndex, "tap", modelData.taps[index])
                            }
                        }
                    }
                }
            }

            RowLayout {
                spacing: 12
                CheckBox {
                    text: "auto latency"
                    checked: root.recorderSettings().auto_latency
                    onToggled: root.updateRecorderSetting("auto_latency", checked)
                }
                CheckBox {
                    text: "transient"
                    checked: root.recorderSettings().transient
                    onToggled: root.updateRecorderSetting("transient", checked)
                }
            }

            RowLayout {
                Label {
                    text: "bit depth"
                    color: root.textColor
                    font.pixelSize: 11
                }
                ComboBox {
                    Layout.preferredWidth: 130
                    font.pixelSize: 11
                    model: ["16-bit", "24-bit", "32-bit float"]
                    currentIndex: {
                        var depth = root.recorderSettings().bit_depth
                        return depth === 16 ? 0 : (depth === 32 ? 2 : 1)
                    }
                    onActivated: {
                        var depths = [16, 24, 32]
                        root.updateRecorderSetting("bit_depth", depths[index])
                    }
                }
                Item { Layout.fillWidth: true }
                Label {
                    text: root.recorderInfo().state + (root.recorderInfo().message ? " · " + root.recorderInfo().message : "")
                    color: root.mutedText
                    font.pixelSize: 10
                    elide: Text.ElideRight
                }
            }

            RowLayout {
                Label {
                    text: "folder"
                    color: root.textColor
                    font.pixelSize: 11
                }
                Label {
                    Layout.fillWidth: true
                    text: root.loopFolder().length > 0 ? root.loopFolder() : "default"
                    color: root.mutedText
                    font.pixelSize: 10
                    elide: Text.ElideMiddle
                }
                FlatButton {
                    text: "browse…"
                    onClicked: loopFolderDialog.open()
                }
            }

            ProgressBar {
                Layout.fillWidth: true
                from: 0
                to: 1
                value: root.recorderInfo().progress || 0
                visible: root.recorderInfo().state === "recording"
            }

            RowLayout {
                FlatButton {
                    text: root.recorderInfo().state === "idle" ? "arm" : "cancel"
                    onClicked: {
                        var state = root.recorderInfo().state
                        root.sendRecorderAction({ action: state === "idle" ? "arm" : "cancel" })
                    }
                }
                FlatButton {
                    text: "loops…"
                    onClicked: loopsWindow.show()
                }
                Item { Layout.fillWidth: true }
                FlatButton {
                    text: "close"
                    onClicked: recorderDialog.close()
                }
            }
        }
    }

    Window {
        id: loopsWindow
        modality: Qt.NonModal
        flags: Qt.Window
        transientParent: root
        visible: false
        width: 760
        height: 430
        minimumWidth: 420
        minimumHeight: 260
        title: "loops"
        color: root.palette.window

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 6

            RowLayout {
                Label {
                    Layout.fillWidth: true
                    text: "loop launcher"
                    color: root.textColor
                    font.pixelSize: 12
                    font.bold: true
                }
                FlatButton {
                    text: "record…"
                    onClicked: root.openRecorder()
                }
                FlatButton {
                    text: "close"
                    onClicked: loopsWindow.close()
                }
            }

            GridView {
                id: loopGrid
                Layout.fillWidth: true
                Layout.fillHeight: true
                cellWidth: 156
                cellHeight: 190
                clip: true
                model: root.loopList()

                delegate: Rectangle {
                    width: 146
                    height: 180
                    color: root.panelColor
                    border.color: root.borderColor
                    border.width: 1

                    ColumnLayout {
                        anchors.fill: parent
                        anchors.margins: 6
                        spacing: 4

                        Label {
                            Layout.fillWidth: true
                            text: modelData.name
                            color: root.textColor
                            font.pixelSize: 11
                            font.bold: true
                            elide: Text.ElideRight
                        }
                        Label {
                            Layout.fillWidth: true
                            text: root.loopDescription(modelData)
                            color: root.mutedText
                            font.pixelSize: 10
                            elide: Text.ElideRight
                        }

                        Canvas {
                            Layout.fillWidth: true
                            Layout.preferredHeight: 32
                            property var peaks: modelData.peaks || []
                            onPeaksChanged: requestPaint()
                            onWidthChanged: requestPaint()
                            onHeightChanged: requestPaint()
                            onPaint: {
                                var ctx = getContext("2d")
                                ctx.clearRect(0, 0, width, height)
                                if (!peaks || peaks.length === 0) return
                                ctx.strokeStyle = root.mutedText
                                ctx.beginPath()
                                for (var i = 0; i < peaks.length; i++) {
                                    var x = i / Math.max(1, peaks.length - 1) * width
                                    var a = peaks[i] * height * 0.5
                                    ctx.moveTo(x, height * 0.5 - a)
                                    ctx.lineTo(x, height * 0.5 + a)
                                }
                                ctx.stroke()
                            }
                        }

                        RowLayout {
                            spacing: 3
                            FlatButton {
                                text: modelData.playing ? "stop" : "play"
                                implicitWidth: 44
                                onClicked: modelData.playing
                                    ? root.stopLoop(modelData)
                                    : root.startLoop(modelData, true)
                            }
                            FlatButton {
                                text: modelData.mute ? "unmute" : "mute"
                                implicitWidth: 50
                                onClicked: root.toggleLoopMute(modelData)
                            }
                            FlatButton {
                                text: "edit"
                                implicitWidth: 40
                                onClicked: loopEditor.openFor(modelData.id)
                            }
                        }

                        Slider {
                            Layout.fillWidth: true
                            implicitHeight: 18
                            from: 0
                            to: 2
                            value: modelData.gain
                            onMoved: {
                                if (!pressed) root.sendRecorderAction({
                                    action: "set_loop_gain",
                                    slot: modelData.id,
                                    gain: value
                                })
                            }
                            onPressedChanged: {
                                if (!pressed) root.sendRecorderAction({
                                    action: "set_loop_gain",
                                    slot: modelData.id,
                                    gain: value
                                })
                            }
                        }
                    }
                }
            }
        }
    }

    Window {
        id: loopEditor
        property int slotId: -1
        property var editorPeaks: []
        property var basePeaks: []
        property var waveSamples: []
        property var waveHf: []
        property var waveTransients: []
        property real viewStart: 0
        property real viewEnd: 0
        property real waveStart: 0
        property real waveEnd: 0
        property real cursorFrame: -1
        property int dragManual: 0
        property bool dragging: false
        property bool navigating: false
        property bool applying: false
        property int waveSeq: 0
        property int acceptedSeq: 0

        function currentSlot() { return root.loopById(slotId) }
        function currentManual() {
            var slot = currentSlot()
            return slot ? slot.manual_nudge : 0
        }
        function displayManual() {
            return (dragging || applying) ? dragManual : currentManual()
        }
        function displayTotal() {
            var slot = currentSlot()
            if (!slot) return 0
            return slot.base_nudge + slot.transient_nudge + displayManual()
        }
        function clampView(start, end) {
            var slot = currentSlot()
            var frames = slot && slot.frames ? slot.frames : 1
            var minSpan = Math.min(frames, 16)
            var span = Math.max(minSpan, Math.min(frames, Math.round(end - start)))
            var s = Math.max(0, Math.min(frames - span, Math.round(start)))
            return { start: s, end: s + span }
        }
        function requestWaveform(nudge, clearPeaks) {
            var slot = currentSlot()
            if (!slot || !slot.frames) return
            if (!(viewEnd > viewStart)) {
                viewStart = 0
                viewEnd = slot.frames
            }
            var view = clampView(viewStart, viewEnd)
            viewStart = Math.round(view.start)
            viewEnd = Math.round(view.end)
            waveSeq += 1
            if (clearPeaks) {
                editorPeaks = []
                waveSamples = []
                waveHf = []
                waveStart = view.start
                waveEnd = view.end
            }
            var full = view.start === 0 && view.end === slot.frames
            root.sendRecorderAction({
                action: "waveform",
                slot: slot.id,
                start: view.start,
                end: view.end,
                nudge: (nudge === undefined || nudge === null) ? null : nudge,
                seq: waveSeq,
                buckets: full ? 16384 : 8192,
                base: false
            })
            editorWave.requestPaint()
            ruler.requestPaint()
        }
        function requestBase() {
            var slot = currentSlot()
            if (!slot || !slot.frames) return
            waveSeq += 1
            root.sendRecorderAction({
                action: "waveform",
                slot: slot.id,
                start: 0,
                end: slot.frames,
                nudge: null,
                seq: waveSeq,
                buckets: 16384,
                base: true
            })
        }
        function fitView() {
            var slot = currentSlot()
            if (!slot) return
            viewStart = 0
            viewEnd = slot.frames
            requestWaveform(null, true)
        }
        function setSpan(span, anchorFrame) {
            var slot = currentSlot()
            if (!slot) return
            var frames = Math.max(1, slot.frames)
            var minSpan = Math.min(frames, 16)
            var next = Math.max(minSpan, Math.min(frames, Math.round(span)))
            var anchor = Math.max(0, Math.min(frames, anchorFrame))
            var start = Math.max(0, Math.min(frames - next, anchor - next * 0.5))
            var view = clampView(start, start + next)
            viewStart = view.start
            viewEnd = view.end
            requestWaveform(null, true)
            ruler.requestPaint()
        }
        function zoomBy(factor, anchorFrac) {
            var slot = currentSlot()
            if (!slot) return
            var frames = Math.max(1, slot.frames)
            var span = Math.max(1, viewEnd - viewStart)
            var anchor = viewStart + anchorFrac * span
            var next = span * factor
            setSpan(next, anchor)
        }
        function zoomToSamples() {
            var slot = currentSlot()
            if (!slot) return
            var center = cursorFrame >= 0 ? cursorFrame : (viewStart + viewEnd) * 0.5
            setSpan(Math.min(slot.frames, 2048), center)
        }
        function nudgeText() {
            var slot = currentSlot()
            if (!slot) return ""
            var total = slot.base_nudge + slot.transient_nudge + displayManual()
            var ms = slot.sample_rate > 0 ? total * 1000 / slot.sample_rate : 0
            return total + " smp (" + (ms >= 0 ? "+" : "") + ms.toFixed(2) + " ms)"
        }
        function cursorText() {
            var slot = currentSlot()
            if (!slot || cursorFrame < 0) return ""
            var frames = Math.max(1, slot.frames)
            var beats = cursorFrame / frames * Math.max(1, slot.loop_beats || 1)
            var bpb = Math.max(1, root.beatsPerBar())
            var bar = Math.floor(beats / bpb) + 1
            var beat = Math.floor(beats % bpb) + 1
            var ms = slot.sample_rate > 0 ? cursorFrame * 1000 / slot.sample_rate : 0
            return bar + "." + beat + " · " + ms.toFixed(1) + " ms · " + Math.round(cursorFrame) + " smp"
        }
        function transientDisplayFrame(raw) {
            var slot = currentSlot()
            if (!slot) return 0
            var frames = Math.max(1, slot.frames)
            return ((raw - displayTotal()) % frames + frames) % frames
        }
        function peakMax(peaks, from, to) {
            var value = 0
            for (var i = Math.max(0, from); i < to && i < peaks.length; i++) {
                value = Math.max(value, peaks[i])
            }
            return value
        }
        function firstTransientFrame() {
            if (waveTransients.length === 0) return -1
            var best = waveTransients[0].frame
            for (var i = 1; i < waveTransients.length; i++) {
                if (waveTransients[i].frame < best) best = waveTransients[i].frame
            }
            return best
        }
        function snapTransient(raw) {
            var slot = currentSlot()
            if (!slot) return
            dragManual = raw - slot.base_nudge - slot.transient_nudge
            applying = true
            root.sendRecorderAction({ action: "snap_transient", slot: slot.id, frame: raw })
            editorWave.requestPaint()
        }
        function snapFirstTransient() {
            var frame = firstTransientFrame()
            if (frame >= 0) snapTransient(frame)
        }
        function snapSeam() {
            var slot = currentSlot()
            if (!slot) return
            applying = true
            root.sendRecorderAction({ action: "snap_seam", slot: slot.id })
        }
        function openFor(id) {
            slotId = id
            editorPeaks = []
            basePeaks = []
            waveSamples = []
            waveHf = []
            waveTransients = []
            viewStart = 0
            viewEnd = 0
            waveStart = 0
            waveEnd = 0
            cursorFrame = -1
            dragManual = 0
            dragging = false
            navigating = false
            applying = false
            waveSeq = 0
            acceptedSeq = 0
            requestBase()
            requestWaveform(null, true)
            visible = true
        }

        Connections {
            target: backend
            function onWaveformChanged() {
                try {
                    var wave = JSON.parse(backend.waveform)
                    if (wave.slot !== loopEditor.slotId) return
                    var slot = loopEditor.currentSlot()
                    var frames = slot ? Math.max(1, slot.frames) : 1
                    var peaks = wave.peaks || []
                    if (wave.base) {
                        loopEditor.basePeaks = peaks
                        editorWave.requestPaint()
                        return
                    }
                    if (wave.seq < loopEditor.acceptedSeq) return
                    if (Math.abs(wave.start - loopEditor.viewStart) > 0.5
                        || Math.abs(wave.end - loopEditor.viewEnd) > 0.5) return
                    loopEditor.acceptedSeq = wave.seq
                    loopEditor.editorPeaks = peaks
                    loopEditor.waveSamples = wave.samples || []
                    loopEditor.waveHf = wave.hf || []
                    loopEditor.waveTransients = wave.transients || []
                    loopEditor.waveStart = wave.start
                    loopEditor.waveEnd = wave.end
                    loopEditor.applying = false
                    editorWave.requestPaint()
                    ruler.requestPaint()
                } catch (e) {}
            }
            function onLoopsChanged() {
                if (loopEditor.visible && !loopEditor.dragging && !loopEditor.navigating) {
                    loopEditor.requestWaveform(null, false)
                }
            }
        }

        modality: Qt.NonModal
        flags: Qt.Window
        transientParent: root
        visible: false
        width: 620
        height: 620
        minimumWidth: 460
        minimumHeight: 520
        title: "loop editor"
        color: root.palette.window

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 6

            Label {
                Layout.fillWidth: true
                text: {
                    var slot = root.loopById(loopEditor.slotId)
                    return slot ? slot.name : ""
                }
                color: root.textColor
                font.pixelSize: 12
                font.bold: true
                elide: Text.ElideRight
            }

            Canvas {
                id: ruler
                Layout.fillWidth: true
                Layout.preferredHeight: 18
                onWidthChanged: requestPaint()
                onHeightChanged: requestPaint()
                onPaint: {
                    var ctx = getContext("2d")
                    ctx.clearRect(0, 0, width, height)
                    var slot = loopEditor.currentSlot()
                    if (!slot || width <= 0) return
                    var frames = Math.max(1, slot.frames || 1)
                    var start = loopEditor.viewStart
                    var end = loopEditor.viewEnd
                    if (!(end > start)) return
                    var span = Math.max(1, end - start)
                    var beats = Math.max(1, slot.loop_beats || 1)
                    var bpb = Math.max(1, Math.round(root.beatsPerBar()))
                    var beatPx = width * (frames / beats) / span
                    ctx.strokeStyle = root.borderColor
                    ctx.fillStyle = root.mutedText
                    ctx.font = "9px monospace"
                    ctx.textBaseline = "middle"
                    for (var b = 0; b <= beats; b++) {
                        var frame = b / beats * frames
                        if (frame < start - 1.0e-6 || frame > end + 1.0e-6) continue
                        var x = (frame - start) / span * width
                        ctx.beginPath()
                        ctx.moveTo(x, 0)
                        ctx.lineTo(x, height)
                        ctx.stroke()
                        var bar = Math.floor(b / bpb) + 1
                        var beat = (b % bpb) + 1
                        if (b % bpb === 0 || beatPx >= 34) {
                            ctx.fillText(bar + "." + beat, x + 2, height * 0.5)
                        }
                    }
                }
            }

            Item {
                Layout.fillWidth: true
                Layout.preferredHeight: 240

                Canvas {
                    id: editorWave
                    anchors.fill: parent
                    property var slot: root.loopById(loopEditor.slotId)
                    onSlotChanged: requestPaint()
                    onWidthChanged: requestPaint()
                    onHeightChanged: requestPaint()
                    onPaint: {
                        var ctx = getContext("2d")
                        ctx.clearRect(0, 0, width, height)
                        if (!slot || width <= 0) return

                        var frames = Math.max(1, slot.frames || 1)
                        var viewStart = loopEditor.waveStart
                        var viewEnd = loopEditor.waveEnd
                        if (!(viewEnd > viewStart)) {
                            viewStart = 0
                            viewEnd = frames
                        }
                        var span = Math.max(1, viewEnd - viewStart)
                        var samples = loopEditor.waveSamples
                        var full = samples.length === 0 && loopEditor.editorPeaks.length === 0
                        var peaks = full
                            ? (loopEditor.basePeaks.length > 0 ? loopEditor.basePeaks : (slot.peaks || []))
                            : loopEditor.editorPeaks
                        if (samples.length === 0 && (!peaks || peaks.length === 0)) return

                        ctx.strokeStyle = root.borderColor
                        ctx.beginPath()
                        ctx.moveTo(0, height * 0.5)
                        ctx.lineTo(width, height * 0.5)
                        ctx.stroke()

                        var total = loopEditor.displayTotal()
                        var phase = ((total % frames) + frames) % frames / frames

                        if (samples.length > 1) {
                            ctx.strokeStyle = root.mutedText
                            ctx.beginPath()
                            for (var n = 0; n < samples.length; n++) {
                                var sx = n / Math.max(1, samples.length - 1) * width
                                var sample = Math.max(-1.5, Math.min(1.5, samples[n]))
                                var sy = height * 0.5 - sample * height * 0.45
                                if (n === 0) ctx.moveTo(sx, sy)
                                else ctx.lineTo(sx, sy)
                            }
                            ctx.stroke()
                        } else {
                            ctx.strokeStyle = root.mutedText
                            ctx.beginPath()
                            for (var x = 0; x < width; x++) {
                                var a = 0
                                if (full) {
                                    var f0 = viewStart + (x / Math.max(1, width)) * span
                                    var f1 = viewStart + ((x + 1) / Math.max(1, width)) * span
                                    var p0 = (((f0 / frames) + phase) % 1 + 1) % 1
                                    var p1 = (((f1 / frames) + phase) % 1 + 1) % 1
                                    var i0 = Math.floor(p0 * peaks.length)
                                    var i1 = Math.floor(p1 * peaks.length)
                                    if (i1 <= i0) i1 = i0 + 1
                                    if (p1 >= p0) {
                                        a = loopEditor.peakMax(peaks, i0, Math.min(peaks.length, i1))
                                    } else {
                                        a = Math.max(
                                            loopEditor.peakMax(peaks, i0, peaks.length),
                                            loopEditor.peakMax(peaks, 0, Math.min(peaks.length, i1))
                                        )
                                    }
                                } else {
                                    var j0 = Math.floor((x / Math.max(1, width)) * peaks.length)
                                    var j1 = Math.max(j0 + 1, Math.floor(((x + 1) / Math.max(1, width)) * peaks.length))
                                    a = loopEditor.peakMax(peaks, j0, Math.min(peaks.length, j1))
                                }
                                var amp = a * height * 0.45
                                ctx.moveTo(x, height * 0.5 - amp)
                                ctx.lineTo(x, height * 0.5 + amp)
                            }
                            ctx.stroke()
                        }

                        // High-pass energy strip: shows where the high-frequency
                        // content is even when the amplitude waveform is saturated.
                        var hf = loopEditor.waveHf
                        if (hf.length > 1) {
                            var hfHeight = 30
                            ctx.fillStyle = Qt.rgba(root.accentColor.r, root.accentColor.g, root.accentColor.b, 0.20)
                            ctx.beginPath()
                            ctx.moveTo(0, 0)
                            for (var h = 0; h < hf.length; h++) {
                                var hx = h / Math.max(1, hf.length - 1) * width
                                var hy = hfHeight - Math.max(0, Math.min(1, hf[h])) * hfHeight
                                ctx.lineTo(hx, hy)
                            }
                            ctx.lineTo(width, 0)
                            ctx.closePath()
                            ctx.fill()
                            ctx.strokeStyle = Qt.rgba(root.accentColor.r, root.accentColor.g, root.accentColor.b, 0.65)
                            ctx.beginPath()
                            for (var h2 = 0; h2 < hf.length; h2++) {
                                var hx2 = h2 / Math.max(1, hf.length - 1) * width
                                var hy2 = hfHeight - Math.max(0, Math.min(1, hf[h2])) * hfHeight
                                if (h2 === 0) ctx.moveTo(hx2, hy2)
                                else ctx.lineTo(hx2, hy2)
                            }
                            ctx.stroke()
                            ctx.fillStyle = root.mutedText
                            ctx.font = "9px monospace"
                            ctx.fillText("HF", 3, hfHeight - 3)
                        }

                        // Beat grid, clipped to the visible window.
                        var beats = Math.max(1, slot.loop_beats || 1)
                        ctx.strokeStyle = Qt.rgba(root.accentColor.r, root.accentColor.g, root.accentColor.b, 0.25)
                        ctx.beginPath()
                        for (var b = 0; b <= beats; b++) {
                            var beatFrame = b / beats * frames
                            if (beatFrame < viewStart - 1.0e-6 || beatFrame > viewEnd + 1.0e-6) continue
                            var bx = (beatFrame - viewStart) / span * width
                            ctx.moveTo(bx, 0)
                            ctx.lineTo(bx, height)
                        }
                        ctx.stroke()

                        // Transient markers, mapped from raw frames by the
                        // current nudge so they follow the rotated waveform.
                        for (var t = 0; t < loopEditor.waveTransients.length; t++) {
                            var marker = loopEditor.waveTransients[t]
                            var display = ((marker.frame - total) % frames + frames) % frames
                            if (display < viewStart - 1.0e-6 || display > viewEnd + 1.0e-6) continue
                            var tx = (display - viewStart) / span * width
                            var strength = Math.max(0.2, Math.min(1.0, marker.strength || 0.5))
                            ctx.strokeStyle = Qt.rgba(root.accentColor.r, root.accentColor.g, root.accentColor.b, 0.25 + 0.55 * strength)
                            ctx.beginPath()
                            ctx.moveTo(tx, 0)
                            ctx.lineTo(tx, height)
                            ctx.stroke()
                            ctx.fillStyle = root.accentColor
                            ctx.beginPath()
                            ctx.moveTo(tx - 3, 0)
                            ctx.lineTo(tx + 3, 0)
                            ctx.lineTo(tx, 5)
                            ctx.closePath()
                            ctx.fill()
                        }

                        if (loopEditor.cursorFrame >= viewStart && loopEditor.cursorFrame <= viewEnd) {
                            var cx = (loopEditor.cursorFrame - viewStart) / span * width
                            ctx.strokeStyle = root.textColor
                            ctx.globalAlpha = 0.4
                            ctx.beginPath()
                            ctx.moveTo(cx, 0)
                            ctx.lineTo(cx, height)
                            ctx.stroke()
                            ctx.globalAlpha = 1.0
                        }
                    }
                }

                Timer {
                    id: requestTimer
                    interval: 30
                    repeat: true
                    running: false
                    onTriggered: {
                        if (!loopEditor.visible) return
                        if (loopEditor.dragging) {
                            loopEditor.requestWaveform(loopEditor.displayTotal(), false)
                        } else if (loopEditor.navigating) {
                            loopEditor.requestWaveform(null, true)
                        }
                    }
                }

                MouseArea {
                    id: waveDrag
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: pressed ? Qt.ClosedHandCursor : Qt.OpenHandCursor
                    property real pressX: 0
                    property real pressY: 0
                    property real pressViewStart: 0
                    property real pressViewEnd: 0
                    property int pressManual: 0
                    property bool moved: false

                    ToolTip.visible: containsMouse && !pressed
                    ToolTip.delay: 500
                    ToolTip.text: "drag: pan / vertical zoom · Alt-drag: nudge (Shift = 1 sample/px) · Ctrl+click transient: snap"

                    onPressed: {
                        var slot = loopEditor.currentSlot()
                        if (!slot) return
                        pressX = mouse.x
                        pressY = mouse.y
                        moved = false
                        if (mouse.modifiers & Qt.AltModifier) {
                            pressManual = loopEditor.displayManual()
                            loopEditor.dragManual = pressManual
                            loopEditor.dragging = true
                            loopEditor.navigating = false
                            loopEditor.applying = false
                            loopEditor.requestWaveform(loopEditor.displayTotal(), false)
                            requestTimer.start()
                        } else {
                            loopEditor.navigating = true
                            loopEditor.dragging = false
                            pressViewStart = loopEditor.viewStart
                            pressViewEnd = loopEditor.viewEnd
                        }
                    }
                    onPositionChanged: {
                        var slot = loopEditor.currentSlot()
                        if (!slot) return
                        var frames = Math.max(1, slot.frames || 1)
                        var viewSpan = Math.max(1, loopEditor.viewEnd - loopEditor.viewStart)
                        loopEditor.cursorFrame = loopEditor.viewStart
                            + (mouse.x / Math.max(1, width)) * viewSpan
                        if (pressed) {
                            var dx = mouse.x - pressX
                            var dy = mouse.y - pressY
                            if (Math.abs(dx) + Math.abs(dy) > 2) moved = true
                            if (loopEditor.dragging) {
                                var span = Math.max(1, loopEditor.waveEnd - loopEditor.waveStart)
                                var delta = (mouse.modifiers & Qt.ShiftModifier)
                                    ? -Math.round(dx)
                                    : -Math.round(dx * span / Math.max(1, width))
                                loopEditor.dragManual = pressManual + delta
                                requestTimer.start()
                            } else if (loopEditor.navigating) {
                                var span0 = Math.max(1, pressViewEnd - pressViewStart)
                                var minSpan = Math.min(frames, 16)
                                var pan = -dx * span0 / Math.max(1, width)
                                // Drag up to zoom in; down to zoom out.
                                var zoom = Math.exp(dy * 0.03)
                                var newSpan = Math.max(minSpan, Math.min(frames, span0 * zoom))
                                var frac = mouse.x / Math.max(1, width)
                                var anchor = pressViewStart + frac * span0 + pan
                                var newStart = Math.max(0, Math.min(frames - newSpan, anchor - frac * newSpan))
                                loopEditor.viewStart = newStart
                                loopEditor.viewEnd = newStart + newSpan
                                loopEditor.waveStart = loopEditor.viewStart
                                loopEditor.waveEnd = loopEditor.viewEnd
                                loopEditor.editorPeaks = []
                                requestTimer.start()
                                ruler.requestPaint()
                            }
                        }
                        editorWave.requestPaint()
                    }
                    onReleased: {
                        requestTimer.stop()
                        var slot = loopEditor.currentSlot()
                        if (loopEditor.dragging) {
                            loopEditor.dragging = false
                            if (slot && loopEditor.dragManual !== slot.manual_nudge) {
                                loopEditor.applying = true
                                root.sendRecorderAction({
                                    action: "set_nudge",
                                    slot: slot.id,
                                    manual: loopEditor.dragManual
                                })
                            } else {
                                loopEditor.applying = false
                            }
                            loopEditor.requestWaveform(null, false)
                        } else if (loopEditor.navigating) {
                            loopEditor.navigating = false
                            if (!moved && (mouse.modifiers & Qt.ControlModifier)) {
                                var total = loopEditor.displayTotal()
                                var frames = Math.max(1, slot ? slot.frames : 1)
                                var span = Math.max(1, loopEditor.viewEnd - loopEditor.viewStart)
                                var best = -1
                                var bestDistance = 8
                                for (var i = 0; i < loopEditor.waveTransients.length; i++) {
                                    var marker = loopEditor.waveTransients[i]
                                    var display = ((marker.frame - total) % frames + frames) % frames
                                    if (display < loopEditor.viewStart || display > loopEditor.viewEnd) continue
                                    var x = (display - loopEditor.viewStart) / span * width
                                    var distance = Math.abs(x - mouse.x)
                                    if (distance < bestDistance) {
                                        bestDistance = distance
                                        best = marker.frame
                                    }
                                }
                                if (best >= 0) loopEditor.snapTransient(best)
                            } else if (moved) {
                                loopEditor.requestWaveform(null, true)
                            }
                        }
                        editorWave.requestPaint()
                        ruler.requestPaint()
                    }
                    onCanceled: {
                        requestTimer.stop()
                        loopEditor.dragging = false
                        loopEditor.navigating = false
                        loopEditor.applying = false
                        loopEditor.requestWaveform(null, true)
                    }
                    onExited: {
                        loopEditor.cursorFrame = -1
                        editorWave.requestPaint()
                    }
                    onWheel: function(wheel) {
                        var delta = wheel.angleDelta.y
                        if (delta === 0) return
                        var factor = Math.pow(1.25, -delta / 120)
                        loopEditor.zoomBy(factor, mouse.x / Math.max(1, width))
                        wheel.accepted = true
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 4
                Label {
                    text: "view"
                    color: root.textColor
                    font.pixelSize: 11
                }
                FlatButton {
                    text: "−"
                    implicitWidth: 26
                    onClicked: loopEditor.zoomBy(2.0, 0.5)
                }
                FlatButton {
                    text: "+"
                    implicitWidth: 26
                    onClicked: loopEditor.zoomBy(0.5, 0.5)
                }
                FlatButton {
                    text: "samples"
                    implicitWidth: 58
                    onClicked: loopEditor.zoomToSamples()
                }
                FlatButton {
                    text: "fit"
                    implicitWidth: 34
                    onClicked: loopEditor.fitView()
                }
                Label {
                    Layout.fillWidth: true
                    text: {
                        var slot = loopEditor.currentSlot()
                        if (!slot) return ""
                        var span = Math.max(1, loopEditor.viewEnd - loopEditor.viewStart)
                        var ms = slot.sample_rate > 0 ? span * 1000 / slot.sample_rate : 0
                        var mode = loopEditor.waveSamples.length > 0 ? "raw" : "envelope"
                        var cursor = loopEditor.cursorText()
                        return mode + " · " + ms.toFixed(2) + " ms · " + Math.round(span) + " smp"
                            + (cursor.length > 0 ? " · " + cursor : "")
                    }
                    color: root.mutedText
                    font.pixelSize: 10
                    elide: Text.ElideRight
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 4
                FlatButton {
                    text: "snap first"
                    enabled: loopEditor.waveTransients.length > 0
                    onClicked: loopEditor.snapFirstTransient()
                }
                FlatButton {
                    text: "zero seam"
                    onClicked: loopEditor.snapSeam()
                }
                Label {
                    Layout.fillWidth: true
                    text: "Ctrl+click a marker to snap the loop start"
                    color: root.mutedText
                    font.pixelSize: 10
                    elide: Text.ElideRight
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 4
                Label {
                    text: "nudge"
                    color: root.textColor
                    font.pixelSize: 11
                }
                FlatButton { text: "-100"; implicitWidth: 40; onClicked: root.nudgeLoop(-100) }
                FlatButton { text: "-10"; implicitWidth: 36; onClicked: root.nudgeLoop(-10) }
                FlatButton { text: "-1"; implicitWidth: 30; onClicked: root.nudgeLoop(-1) }
                Label {
                    Layout.fillWidth: true
                    text: loopEditor.nudgeText()
                    color: root.textColor
                    font.pixelSize: 11
                    font.family: "monospace"
                    horizontalAlignment: Text.AlignHCenter
                }
                FlatButton { text: "+1"; implicitWidth: 30; onClicked: root.nudgeLoop(1) }
                FlatButton { text: "+10"; implicitWidth: 36; onClicked: root.nudgeLoop(10) }
                FlatButton { text: "+100"; implicitWidth: 40; onClicked: root.nudgeLoop(100) }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 4
                Label {
                    text: "manual"
                    color: root.textColor
                    font.pixelSize: 11
                }
                SpinBox {
                    from: -10000000
                    to: 10000000
                    editable: true
                    value: loopEditor.displayManual()
                    onValueModified: {
                        loopEditor.dragManual = value
                        loopEditor.applying = true
                        var slot = loopEditor.currentSlot()
                        if (slot) root.sendRecorderAction({
                            action: "set_nudge",
                            slot: slot.id,
                            manual: value
                        })
                        editorWave.requestPaint()
                    }
                }
                Item { Layout.fillWidth: true }
                Label {
                    text: "samples"
                    color: root.mutedText
                    font.pixelSize: 10
                }
            }

            Label {
                Layout.fillWidth: true
                text: {
                    var slot = root.loopById(loopEditor.slotId)
                    if (!slot) return ""
                    return "auto " + slot.base_nudge + " · transient " + slot.transient_nudge
                        + " · manual " + loopEditor.displayManual()
                }
                color: root.mutedText
                font.pixelSize: 10
            }

            RowLayout {
                Layout.fillWidth: true
                Label {
                    text: "gain"
                    color: root.textColor
                    font.pixelSize: 11
                }
                Slider {
                    Layout.fillWidth: true
                    from: 0
                    to: 2
                    value: {
                        var slot = root.loopById(loopEditor.slotId)
                        return slot ? slot.gain : 1
                    }
                    onMoved: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (slot && !pressed) root.sendRecorderAction({
                            action: "set_loop_gain",
                            slot: slot.id,
                            gain: value
                        })
                    }
                    onPressedChanged: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (slot && !pressed) root.sendRecorderAction({
                            action: "set_loop_gain",
                            slot: slot.id,
                            gain: value
                        })
                    }
                }
                CheckBox {
                    text: "mute"
                    checked: {
                        var slot = root.loopById(loopEditor.slotId)
                        return slot ? slot.mute : false
                    }
                    onToggled: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (slot) root.sendRecorderAction({ action: "set_loop_mute", slot: slot.id, mute: checked })
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Label {
                    text: "name"
                    color: root.textColor
                    font.pixelSize: 11
                }
                TextField {
                    Layout.fillWidth: true
                    text: {
                        var slot = root.loopById(loopEditor.slotId)
                        return slot ? slot.name : ""
                    }
                    onEditingFinished: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (slot) root.renameLoop(slot, text)
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Label {
                    text: "folder"
                    color: root.textColor
                    font.pixelSize: 11
                }
                Label {
                    Layout.fillWidth: true
                    text: root.loopFolder().length > 0 ? root.loopFolder() : "default"
                    color: root.mutedText
                    font.pixelSize: 10
                    elide: Text.ElideMiddle
                }
                FlatButton {
                    text: "browse…"
                    onClicked: loopFolderDialog.open()
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Label {
                    text: "bit depth"
                    color: root.textColor
                    font.pixelSize: 11
                }
                ComboBox {
                    id: editorDepth
                    Layout.preferredWidth: 120
                    font.pixelSize: 11
                    model: ["16-bit", "24-bit", "32-bit float"]
                    currentIndex: {
                        var slot = root.loopById(loopEditor.slotId)
                        var depth = slot ? (slot.bit_depth || root.recorderSettings().bit_depth) : 24
                        return depth === 16 ? 0 : (depth === 32 ? 2 : 1)
                    }
                }
                FlatButton {
                    text: "save"
                    onClicked: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (!slot) return
                        var depths = [16, 24, 32]
                        root.saveLoop(slot, depths[editorDepth.currentIndex])
                    }
                }
                Label {
                    Layout.fillWidth: true
                    text: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (!slot) return ""
                        return slot.saved ? (slot.path || "saved") : "not saved"
                    }
                    color: root.mutedText
                    font.pixelSize: 10
                    elide: Text.ElideMiddle
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Item { Layout.fillWidth: true }
                FlatButton {
                    text: "delete"
                    onClicked: {
                        var slot = root.loopById(loopEditor.slotId)
                        if (slot) root.sendRecorderAction({ action: "loop_delete", slot: slot.id })
                        loopEditor.close()
                    }
                }
                FlatButton {
                    text: "close"
                    onClicked: loopEditor.close()
                }
            }
        }
    }
}
