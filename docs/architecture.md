# vibemix architecture

`vibemix` is a JACK/ PipeWire audio mixer with LV2 plugin support, split into an
audio engine and a Qt Quick (QML) control surface.

```
┌─────────────────────────┐         OSC (localhost UDP)        ┌──────────────────────────┐
│  vibemix-engine (Rust)  │  params/config ────────────────▶    │   vibemix-gui (Rust +    │
│  - JACK client (RT)     │  ◀──────────── meters/state        │   cxx-qt / Qt Quick QML) │
│  - mixer graph          │                                    │   - QML surface          │
│  - LV2 host (lilv)      │                                    │   - Qt event loop        │
│  - control server       │                                    │   - OSC client (Rust)    │
└───────────┬─────────────┘                                    └──────────────────────────┘
            │ JACK ports
      ┌─────▼──────────────────────────────────────┐
      │ system capture / playback, other JACK apps │
      └────────────────────────────────────────────┘
```

Only the engine is a JACK client. The GUI never touches audio; a stalled GUI
cannot cause audio dropouts.

## Crates

| Crate | Responsibility |
|---|---|
| `vibemix-core` | Shared model (strips, buses, sends, plugin slots), ids, params, and the `AudioBackend` / `ProcessHandler` traits |
| `vibemix-proto` | OSC address scheme and `ControlMessage` encode/decode |
| `vibemix-lv2` | LV2 hosting: `urid:map`/`urid:unmap` host feature, plugin discovery, instances (audio, control in/out, atom sequences), run |
| `vibemix-engine` | JACK client, RT mixer graph, LV2 host, control server |
| `vibemix-gui` | cxx-qt QML application and OSC client |

## Audio backend

The engine talks to JACK (provided by PipeWire) through the `jack` crate. The
running implementation is `backend_jack::JackEngine`: it registers the strip and
master ports, owns the `ProcessHandler`, and links against PipeWire's JACK
library.

The `AudioBackend` trait in `vibemix-core` is a provisional seam. Its current
shape does not yet carry RT port buffers, so the engine uses the concrete
`JackEngine` for now; the trait would be finalized if a second backend (a native
PipeWire `pw_filter`) is added (unscheduled; see the roadmap).

## Real-time safety rules

- No heap allocation, freeing, locking, syscalls, or panics inside the JACK
  callback. Audio/accumulator buffers are sized once (`max(buffer, 8192)`) and
  never resized; a longer period is truncated (bus outputs are zeroed).
- Parameter changes arrive through an SPSC ring (`rtrb`) and are applied at block
  start. Scalar parameters (gain, mute, pan, send gain/delay) use atomics with
  one-pole smoothing, so live changes don't click.
- Structural changes (add/remove strip, plugin, chain swap) are built on the
  control thread and swapped in at a block boundary; retired plugin instances
  and delay lines are handed back over garbage rings and dropped off the RT
  thread (queues are sized so a full command queue's worth can never overflow;
  on the pathological full case the object is leaked rather than freed).
- Sample-rate changes are detected by a JACK notification handler and rebuild
  delay-line buffers + plugin instances on the control thread (`SetSampleRate`).
- FTZ/DAZ are enabled on the process thread so plugin tails don't fall into
  denormals. JACK xruns are counted and surfaced in the GUI.
- LV2 worker communication uses a dedicated non-RT worker thread.
- Set `VIBEMIX_RT_GUARD=1` to abort (debug) if any heap operation happens on the
  audio thread — used when testing/auditing.

## Mixer model

The engine is built with a fixed capacity of strips and buses (CLI `--strips`,
`--buses`; default 8 and 4). Strip input port pairs and bus output port pairs are
registered only for active strips/buses and unregistered when they are removed,
so the JACK graph only shows what exists. Adding/removing strips and buses at
runtime still just toggles `active` slots for the mixer graph. Audio flow:

```
strip inputs -> [input trim] -> [pre-fader inserts] -> [fader/pan/mute/solo] -> [post-fader inserts] -> sends[] -> bus accumulators -> [bus gain/mute] -> bus outputs
```

Each strip has a dB input trim (`-24..+24 dB`, unity at 0) applied before the
plugin chain, so a low input can be brought up to a plugin's operating level (and
hot inputs attenuated). All levels are measured as **peak dBFS** (`20·log10`);
the strip has three meter taps — `in` (raw input), `gain` (post-trim, pre-plugin)
and `out` (post-fader/pan) — and the GUI can select which one each strip's meter
shows. The numeric readout and the meter's dB scale use `-60..+6 dBFS`.

Each strip has one send per bus (`strip.sends`), exposed as a slider per bus in
the GUI and tapped post-fader by default; a send flagged `pre_fader` taps the
signal after the pre-fader insert chain but before the fader and pan. Each send
also carries an alignment delay (`delay_ms`,
`0..500 ms`, smoothed and linearly interpolated) so a signal routed through
several buses with different latencies (a dry path vs. a latency-inducing FX
bus) can be phase-aligned per send. The GUI entry accepts `ms`, seconds (`s`) or
samples (`smp`/`samples`); a bare number is milliseconds, and the value is shown
with 3-decimal ms plus a samples tooltip. Bus 0 is the master and is
auto-connected to system playback.
Each bus sums its incoming sends, runs its own LV2 insert chain, applies
gain/mute, and drives its JACK output ports (`{slug}_out_l/r`) — so a bus can be a
hardware tap (sampling) or a monitor out. A bus can also **mix to master**
(`mix_to_master`), which routes its processed output into bus 0; together with
sends this gives submix-with-effects and plugin-aux routing.

A non-master bus can also **return** an external effect (`return_inputs`), which
turns it into an external insert:

```
sum of sends -> bus inserts -> {slug}_out_l/r   (pre-fader send, x send_gain)
             -> {slug}_ret_l/r x return_gain -> mixed back in (dry + wet)
             -> bus gain/mute -> master (only when mix_to_master)
```

The bus fader is applied *after* the device, so it sets the returned level.
How hard the device is driven is set by the per-strip send levels and the
**SEND** knob (`send_gain`, `0..+12 dB`, smoothed) that trims the pre-fader send
to the device; the **RET** knob (`return_gain`) trims the device's return before
it is mixed back in. Since the return only reaches master through
`mix_to_master`, enabling **External return** in the GUI also enables **Mix to
master**. The bus VU-meter cycles four taps: `in` (sum of sends), `send` (device
drive, post-insert x SEND), `ret` (device return, post RET) and `out` (post-fader
dry + wet). The master has no separate send, so it cannot return externally.
Strip 0..3 inputs are auto-connected to `system:capture_1..8` when present.

Port short names follow the strip/bus name: strips expose `{slug}_in_l` /
`{slug}_in_r`, buses `{slug}_out_l` / `{slug}_out_r` and (when returning)
`{slug}_ret_l` / `{slug}_ret_r`, where `{slug}` is the name
with whitespace and `:` replaced by `_` (capped at 48 chars). Duplicate names get
an `_{id}` suffix so port names stay unique. Renaming a strip or bus renames its
ports live (the number of ports registered is unchanged, and existing connections
survive the rename).

Sessions remember external JACK connections: on save the engine records the ports
feeding each strip input, fed by each bus output, and feeding each bus return
(`StripConnections` / `BusConnections`), and on load it restores them by name. Restoration is
authoritative for any strip/bus that has saved patch info (its ports are
disconnected first), while entities without info are left alone so auto-connect
still applies. Connections whose other end is absent at load time are reported to
the GUI over `/vibemix/connections/missing`, which shows a popup listing them.

## LV2 hosting

`vibemix-lv2` wraps `lilv` and supplies the host features that `lilv` 0.2.4
omits. The engine hosts per-strip insert chains (pre- and post-fader) and a
per-bus insert chain from the session and at runtime. On the development box the
whole installed mono/stereo set (226 plugins: LSP, Calf, x42, meters.lv2, …)
passes the isolated smoke sweep; see "Plugin health" below.

Plugins are discovered by scanning the standard LV2 directories (`lilv`'s
`World::load_all`), keeping the hostable mono/stereo ones; the list is persisted
to `$XDG_CONFIG_HOME/vibemix/catalog.json` (full plugin metadata) on startup and
after each scan. The GUI's plugin picker (a modal window with a name filter) can
trigger a rescan (`/vibemix/plugins/rescan`), which builds a fresh `lilv` world —
the previous worlds are kept alive so instances/UI windows created from them stay
valid — re-discovers, rewrites the catalog, and rebuilds the chains. A plugin
that cannot be instantiated (uninstalled or refused by the smoke test) is kept
in the session,
bypassed (its DSP slot is a pass-through so slot indices stay aligned), and its
name is shown red in the plugin list; it is re-instantiated when it becomes
available again.

Audio path per strip:

```
input -> input trim -> pre-fader inserts (stereo) -> gain/pan -> post-fader inserts -> sends (x gain, + delay) -> bus accumulators
```

Bus path:

```
sum of sends -> bus inserts (stereo) -> bus gain/mute -> output ports + optional mix to master
```

A bus with an external return (`return_inputs`, non-master only) is an insert:
the post-insert signal leaves `{slug}_out_l/r` pre-fader for the device (scaled
by `send_gain`), and the device output on `{slug}_ret_l/r` is scaled by
`return_gain` and mixed back in with the dry signal, then passed through the bus
fader/mute:

```
{slug}_out_l/r (device send x SEND) -> device -> {slug}_ret_l/r x RET -> dry + wet -> bus fader/mute -> master (if mix_to_master)
```

- Mono (1-in/1-out) plugins are run as two instances (one per channel); stereo
  (2-in/2-out) plugins as one. Other channel counts are rejected.
- Up to `MAX_PLUGINS = 6` inserts **per chain** and `MAX_PARAMS = 64` controls each.
  Each strip has two chains: pre-fader inserts (between the input trim and the
  fader) and post-fader inserts (after fader/pan, before the sends). The OUT meter
  is measured after the post-fader inserts.
- Plugin parameters live in lock-free atomics; the RT thread copies them into the
  plugin's control buffers each block. Values are clamped to the port's declared
  `minimum`/`maximum` (`supportsStrictBounds` safe). Port metadata is read
  whether the RDF literal is a float, integer, or boolean (some plugins write
  `lv2:default 1 ;`, which must not be mistaken for "no default").
- Adding/removing plugins never deactivates the JACK client (that would drop all
  port connections). Instead, changes are sent to the RT thread over `rtrb`
  SPSC queues and applied at a block boundary; retired plugin instances are
  sent back to the control thread to be dropped off the RT thread.
- Each insert can be bypassed at runtime; the RT thread skips processing for it
  and lets the signal pass through.

Host features provided: `urid:map`/`urid:unmap` (intern table, leaked with a
stable address), `buf-size:boundedBlockLength`, and `options:options` carrying
`sampleRate`, `minBlockLength`, `maxBlockLength`, `nominalBlockLength`, and
`sequenceSize`. Ports supported: audio, control input/output, and atom
`Sequence` input/output; output-capable atom ports advertise their capacity in
`atom.size`.

Block-size policy: JACK fixes the period for the life of a client (changing it
is a reconfiguration), while PipeWire may vary the quantum. The host advertises
a generous `maxBlockLength` (8192 frames) and passes the current period as
`nominalBlockLength` at instantiate. Host scratch buffers track the live period;
if it ever exceeds the advertised max, plugins process only the first max frames
and the remainder passes through.

`worker:schedule` is supported: plugins that require it (x42 IR/preset
convolvers) get a per-instance worker thread with lock-free request/response
queues; responses are delivered on the RT thread. Plugins that merely opt in
also work.

LV2 `state` save/restore is supported. Saving a session captures each plugin's
state store (key/value/type/flags) into the session file; restoring applies it
when the plugin is instantiated. Save runs under a brief cooperative pause so
the plugin's `save()` never races the RT callback, and `state:mapPath` is
provided so file-path state (e.g. convolution IRs) round-trips.

Native plugin UIs: editors are hosted in their own floating windows via `suil`
(no embedding into the QML window). X11 UIs (LSP, x42, …) appear through
XWayland; GTK2/GTK3 UIs (e.g. Calf) are hosted in a `GtkWindow`. Open/close from
the GUI "UI" button (`/vibemix/plugin/ui` and `/vibemix/plugin/ui/close`). A
plugin offering both X11 and GTK is hosted as X11 (preferred).

LV2 editors are hosted **out of process** by default: the engine spawns
`vibemix-engine --ui-host <socket>` (one shared host) and talks to it over a
Unix socket (`vibemix-lv2::ui_ipc`). The host runs the same `ui_host.c` window
code; the engine forwards output values (`ui:portEvent`) and the host forwards
parameter edits. This matters because the PipeWire data-loop runs **inside the
engine process**, so constructing a toolkit UI there stalls the data-loop and
causes xruns; in the host process it cannot. A UI that declares
`lv2:requiredFeature instance-access` reads the DSP instance directly and cannot
be remoted, so it (and every UI when `VIBEMIX_UI_LOCAL=1`) falls back to
in-process hosting, which keeps the subprocess smoke-test gate below. The host
is spawned lazily on the first remote open, hosts any number of windows, and is
shut down (or exits on socket EOF) with the engine; a host crash drops its
windows without affecting audio.

GTK is an **optional runtime dependency**, loaded with `dlopen()` and skipped
when absent, so the engine runs on a system with no GTK at all. GTK2 and GTK3
cannot be initialized in the same process, so exactly one major version is
chosen: by default the version that actually has hostable plugin UIs (`ui:GtkUI`
= GTK2, `ui:Gtk3UI` = GTK3; Calf → GTK2), overridable with
`VIBEMIX_GTK=gtk2|gtk3`. GTK hosting advertises `ui:makeResident` (Calf requires
it) and `ui:windowTitle`. Installing a GTK plugin after the catalog was built
needs `/vibemix/plugins/rescan`.

The host provides `ui:resize`, `ui:showInterface` (with the retained
`LV2UI_Resize` kept alive for the UI), and `options:options` carrying
`ui:updateRate` and `ui:scaleFactor`. It pumps every UI — including its
`ui:idleInterface` — each control-loop iteration, so animated UIs (including
LSP's OpenGL UIs) update.

Each plugin window is created with `WM_CLASS=vibemix-gui` (matching
`StartupWMClass` in `packaging/vibemix.desktop`), `_NET_WM_PID`, and
`_NET_WM_NAME` set to the plugin's name, so desktop environments group it under
the vibemix application/icon rather than a generic one.

The main GUI window takes the other route: it calls
`QGuiApplication::setDesktopFileName("vibemix")`, so the Wayland `app_id`
(and the X11 `_KDE_NET_WM_DESKTOP_FILE` property) match
`packaging/vibemix.desktop` and the compositor resolves `Icon=vibemix`. Without
it Qt falls back to the executable basename (`vibemix-gui`), for which no
desktop entry exists, and the window gets a generic icon. `StartupWMClass`
cannot cover this on Wayland: the icon is looked up from the desktop file
identified by `app_id`, not from `WM_CLASS`.

The engine calls `XInitThreads()` before its first Xlib call. Plugin UIs such
as LSP's X11/GL toolkit use Xlib from multiple threads and call `XInitThreads`
themselves, but that is only effective if it happens first; otherwise their
UIs can freeze.

Plugin **output** ports are forwarded to each open UI as `ui:portEvent` from
the control loop (control outputs every pump, atom outputs on change). The
audio thread publishes output snapshots into a lock-free structure shared with
the control thread (a `SharedOutputs` with atomics for control ports and a
seqlock for atom payloads); publishing is gated on a per-instance "UI open"
flag, so the audio thread does no extra work when no UI is open. Host-driven
parameter edits are mirrored back into the native UI the same way.

In-process (fallback) UIs are gated by a short subprocess smoke test as a safety
net: a UI that crashes or hangs is refused rather than taking the engine down.
The smoke test runs headless (the window is never mapped) and also exercises the
output-forwarding path, so it produces no visible flash. Out-of-process UIs need
no smoke — the separate process already isolates them.

VST3 editors are hosted separately: the plugin's `IPlugView` is embedded into a
raw X11 window (`kPlatformTypeX11EmbedWindowID`) created per editor, with a
minimal `IPlugFrame` handling plugin-driven resizes and the window's close
button. Host-driven parameter edits reach the editor through
`IEditController::setParamNormalized`, and moves made in the editor are folded
back into the shared parameter state via an `IComponentHandler::performEdit`
queue (mirroring the LV2 `ui:portEvent` write-back), so they survive session
saves and are pushed to the DSP. `restartComponent` is observed as well:
`kParamValuesChanged` resyncs the shared values, and latency/IO/reload requests
are logged. Opening an editor is gated by a headless child smoke test
(`--smoke-vst3-ui`), and the editor button is only shown when the plugin
reports an X11-editable view.

Every audio bus the plugin declares is presented during processing: bus 0 is
wired to the strip/bus signal, extra input buses (e.g. a sidechain) are fed
silence, and extra output buses get throwaway scratch, so a plugin's bus
topology is preserved even though vibemix only routes the main bus.

**Sidechain routing.** Both formats expose sidechains: VST3 via aux input
buses, LV2 via extra audio input ports. LV2 sidechain ports are found by tiered
detection — explicit `pg:sideChainOf`, else the `pg:mainInput` group (ungrouped
extras are sidechain, as in Calf), else the first `outputs` inputs are main and
ungrouped extras that are optional or named like a sidechain are sidechain;
plugins whose main/extras cannot be told apart (A/B testers, mixers) are
refused. A plugin that has a sidechain can have it **enabled** from its context
menu (off by default — some plugins expose a sidechain they never use). Once
enabled it becomes a **send destination**, shown (under a short `sc` label) on
every strip like a bus send: each strip has a row with its own gain and pre/post
tap (default pre-fader), and several strips can feed one sidechain at different
levels. The sources are stored on the plugin
(`PluginSlot.sidechain: Vec<SidechainSource>`, keyed by stable strip id, so they
travel with the plugin), and summed into its first non-CV aux input instead of
silence. The plugin's own strip row is shown but disabled (keeps strips
aligned, avoids a feedback path).

To avoid the variable PipeWire quantum becoming an audible sidechain delay, the
engine processes strips in a **topological order** (recomputed per change) that
runs a plugin's source strips before the strip that consumes them, so the tap
is complete in the same block; strips left in a dependency cycle read the
previous block's tap. The order is installed via an RT command and the old
order is retired on the control thread.

Not supported: Qt plugin UIs (and GTK4).

This hosts all mono/stereo plugins, including meters/analysers with atom output
ports.

Safety: arbitrary third-party plugins can crash the process, so before a plugin
is added the engine re-execs itself with `--smoke <uri>`, which instantiates it
offline and runs a few blocks; if the child crashes or times out the add is
refused. Discovery also refuses port shapes we cannot fully connect.
`cargo run -p vibemix-lv2 --example probe` smoke-tests discovery/instantiate;
`--example scan_silence` checks every hostable plugin passes audio with its
defaults; `--example run_plugin <uri>` runs one plugin offline.

### Plugin health

The engine sweeps every hostable mono/stereo LV2 plugin (and every VST3 effect)
in an isolated child process and records the verdict, rather than hiding
suspected crashers behind a blacklist. `vibemix-engine --scan-plugins` loads the `lilv` world once, `fork()`s
per plugin (the child is single-threaded, so this is safe), runs the same
offline block test, and prints one JSON verdict per line (`ok` /
`crash SIGSEGV` / `timeout` / `instantiate failed`). At startup the engine runs
a full sweep when the cache is missing or its host ABI version changed, and
otherwise a **targeted** sweep for just the newly installed plugins that have no
verdict yet (`--scan-plugins --only <uri>…`). A plugin rescan also health-checks
anything it newly discovers, and the picker's "test all"
(`/vibemix/plugins/retest`) forces a full sweep. The
supervisor runs in its own process group at low priority (`setpgid` +
`setpriority`), so the audio thread and UI always win; a sweep can be aborted
wholesale (`/vibemix/plugins/scan/cancel`, Esc in the GUI), which is useful if a
collection is huge or a third-party plugin makes the sweep hang. The sweep is a
separate process, so it never touches the RT graph; the top status bar shows a
thin progress strip with a `testing plugins done/total` counter while it runs.

Verdicts are cached in `$XDG_CONFIG_HOME/vibemix/plugin_health.json`, tagged with
`HOST_ABI_VERSION` (`vibemix-lv2`); bump it when host behavior changes so fixed
plugins are retried. The catalog carries `status` (`ok`/`failed`/`unsupported`)
and a human `reason`; the picker paints failed/unsupported entries red (with the
reason) and has a "hide failed" toggle. Entries stay clickable — the smoke guard
still refuses anything that crashes. A plugin requiring a feature the host does
not implement is listed as `unsupported` with that feature's name. The only
remaining blacklist is the optional operator escape hatch
`VIBEMIX_LV2_BLACKLIST=uri1,uri2`.

Three host bugs surfaced by the sweep and fixed here: `lv2_raw` types
`LV2_Descriptor::extension_data` as required, so a plugin that (legally) leaves
it NULL made `lilv`'s shim call a null pointer (EQ10Q); output
`atom:Sequence` ports were not re-armed with their capacity between blocks, so
atom-output meters (bitscope, signal-distribution histogram) asserted on the
second block; and `PluginInstance` dropped the LV2 instance (freeing it and
`dlclose`-ing the plugin library) before joining the worker thread, so a plugin
still executing `work()` on that thread (Gxdetune resizing its FFTW plans) ran
code from an unloaded library (`Drop for PluginInstance` now joins the worker
first). `--debug` (or `VIBEMIX_DEBUG=1`) logs the child's signal and keeps its
stderr; smoke children always run with core dumps disabled.

## Control protocol

OSC over localhost UDP. See `vibemix-proto::address`:

| Address | Args |
|---|---|
| `/vibemix/strip/gain` | `int strip, float gain` |
| `/vibemix/strip/mute` | `int strip, int mute` |
| `/vibemix/strip/solo` | `int strip, int solo` |
| `/vibemix/strip/pan` | `int strip, float pan` |
| `/vibemix/strip/input_gain` | `int strip, float db` (input trim, `-24..+24 dB`) |
| `/vibemix/strip/name` | `int strip, string name` |
| `/vibemix/strip/add` | none |
| `/vibemix/strip/remove` | `int strip` |
| `/vibemix/strip/move` | `int strip, int to_index` (display order only) |
| `/vibemix/strip/duplicate` | `int strip` (clone with plugins) |
| `/vibemix/bus/gain` | `int bus, float gain` |
| `/vibemix/bus/mute` | `int bus, int mute` |
| `/vibemix/bus/name` | `int bus, string name` |
| `/vibemix/bus/move` | `int bus, int to_index` (master stays last) |
| `/vibemix/bus/mix_master` | `int bus, int on` |
| `/vibemix/bus/return` | `int bus, int on` (register/remove `{slug}_ret_l/r`) |
| `/vibemix/bus/send_gain` | `int bus, float gain` (pre-fader send to an external device) |
| `/vibemix/bus/return_gain` | `int bus, float gain` (trim on the device's return) |
| `/vibemix/bus/add` | none |
| `/vibemix/bus/remove` | `int bus` |
| `/vibemix/bus/duplicate` | `int bus` (master cannot be duplicated) |
| `/vibemix/send/gain` | `int strip, int bus, float gain` |
| `/vibemix/send/delay` | `int strip, int bus, float ms` (0..500, alignment delay) |
| `/vibemix/send/pre_fader` | `int strip, int bus, int pre` (tap before fader/pan) |
| `/vibemix/plugin/add` | `int location, int target, string uri` |
| `/vibemix/plugin/remove` | `int location, int target, int slot` |
| `/vibemix/plugin/move` | `int location, int target, int from, int to` |
| `/vibemix/plugin/param` | `int location, int target, int slot, int param, float value` |
| `/vibemix/plugin/bypass` | `int location, int target, int slot, int bypassed` |
| `/vibemix/plugin/sidechain/enabled` | `int location, int target, int slot, int enabled` |
| `/vibemix/plugin/sidechain/send` | `int location, int target, int slot, int strip, float gain` |
| `/vibemix/plugin/sidechain/pre_fader` | `int location, int target, int slot, int strip, int pre` |
| `/vibemix/plugin/ui` | `int location, int target, int slot` (open native UI window) |
| `/vibemix/plugin/ui/close` | `int location, int target, int slot` |
| `/vibemix/plugin/copy` | `int location, int target, int slot` |
| `/vibemix/plugin/cut` | `int location, int target, int slot` |
| `/vibemix/plugin/paste` | `int location, int target` (append to that chain) |
| `/vibemix/plugin/clipboard` | `int has, int location, string name` (engine to GUI) |
| `/vibemix/state/query` | none |
| `/vibemix/state` | `string json` (engine to GUI: strips/buses with plugins + control metadata) |
| `/vibemix/catalog` | `string json` (engine to GUI: hostable plugins) |
| `/vibemix/plugins/rescan` | none (re-scan LV2 dirs, refresh catalog, rebuild chains) |
| `/vibemix/plugins/retest` | none (start/refresh the isolated plugin health sweep) |
| `/vibemix/plugins/scan/cancel` | none (abort a running/hung sweep; kills the whole process group) |
| `/vibemix/plugins/scan` | `int running, int done, int total` (engine to GUI: sweep progress) |
| `/vibemix/session/save` | `string path, int width, int height, int maximized` (window geometry saved with the session) |
| `/vibemix/session/load` | `string path` |
| `/vibemix/session/new` | none (reset to a minimal default session) |
| `/vibemix/session/result` | `int ok, string path, string error` (engine to GUI: save/load/new outcome) |
| `/vibemix/edit/undo` | none |
| `/vibemix/edit/redo` | none |
| `/vibemix/edit/state` | `int undo_depth, int redo_depth, string undo_label, string redo_label` (engine to GUI) |
| `/vibemix/connections/missing` | `string json` (engine to GUI: saved connections that could not be restored) |
| `/vibemix/meters` | `int strip_count, in_l…, in_r…, gain_l…, gain_r…, out_l…, out_r…, int bus_count, buses_l…, buses_r…` (linear peak levels, engine to GUI, ~50 Hz) |

Meters and state snapshots are sent to the last address that sent a control
message. State snapshots also carry `window`, `sample_rate` and the `xruns`
count. Sessions are TOML-serialized `MixerModel` files, including plugin URIs
and parameter values and the last window size/maximized (`window`). Reordering
strips/buses (`*/move`) permutes the model's display order only: engine slots,
ports and parameters are keyed by id, so audio routing is unaffected. The master
(bus 0) is pinned to the back (rightmost).

The GUI sends its window geometry with `/vibemix/session/save`; on load the
engine reports it back inside the state snapshot (`window`), and the GUI resizes
itself (old sessions without it leave the window unchanged).

The GUI exposes a File menu (New/Open/Save/Save As/Recent/Quit) backed by
`QtQuick.Dialogs` file dialogs, defaulting to `$XDG_CONFIG_HOME/vibemix/sessions`,
and warns about unsaved changes (tracked by comparing state snapshots against the
last saved state) before New/Open/Quit. An Edit menu and `Ctrl+Z`/`Ctrl+Shift+Z`
drive undo/redo (labels come from `/vibemix/edit/state`); plugin context menus
offer Add plugin…, Copy/Cut/Paste (labelled with the clipboard contents), and
strip/bus context menus offer Duplicate. Right-clicking a plugin list — a row or
the blank area of an empty/partly-filled chain — opens the same plugin menu, so
there is no separate "+" add button.

Plugin messages address a chain with `location` — `0` strip pre-fader, `1` strip
post-fader, `2` bus insert — and `target` (the strip or bus id).

## Milestones

- **Phase 0** - prerequisites + spikes (JACK passthrough, lilv host, cxx-qt QML window). Done.
- **Phase 1** - thin vertical slice: one strip to master, fader + meter end to end. Done.
- **Phase 2** - full strip/bus model, sends, mute/solo, session save/load. Done.
- **Phase 3** - LV2 hosting: discovery + instances (`vibemix-lv2`), per-strip
  insert chains, parameter control and UI, and session persistence. Done
  (mono/stereo plugins; control params; `worker:schedule`, `options`, state and
  native UI windows all supported).
- **Phase 4** - RT hardening (fixed buffers, no RT alloc/free, denormals,
  smoothing, sample-rate-change handling, xrun counter, alloc guard), lock-free
  graph swap, and source-install packaging (`just install`, desktop entry/icon).
  Done (no AppImage/Flatpak).
- **Phase 5** - editor quality-of-life: single-plugin clipboard (copy/cut/paste,
  chain-aware), duplicate strip/bus, and snapshot-based undo/redo. Done.
- **Phase 6** - MIDI: clock-slave sync (tempo/transport from incoming MIDI
  clock), LV2 host tempo (`time:Position`/`time:tempo`), a MIDI control surface
  (CC to faders/params, learnable, saved with the session), transport/position
  counters, and a mono/stereo plugin-picker filter. Done. See below.
- **Phase 6 follow-ups** - plugin health: an isolated fork-per-plugin sweep that
  replaces the blacklist (persisted verdicts, red picker entries, test-all /
  cancel), plus three host-correctness fixes it surfaced (NULL
  `extension_data`, output-atom re-arm, worker joined before instance free);
  and mixer-UI polish (wider centred cards, reference mini-sliders for
  trim/pan/send/return, entry-font fixes). Done. See "LV2 hosting" / "Plugin
  health".
- **Unscheduled** - a native PipeWire `pw_filter` backend (finalizing the
  `AudioBackend` trait). No arbitrary module graph / port matrix is planned; the
  strip/bus/preset mixer topology is the design.

### Phase 6 - MIDI

The mixer is a MIDI **clock slave** (computers make poor clock masters): it
follows an incoming 24 PPQN MIDI clock to derive tempo and transport, and never
generates clock. A single JACK `MidiIn` port carries clock, transport, notes and
CC; a matching `MidiOut` (feedback) is deferred.

- **Host tempo:** each block a `time:Position` atom (`frame`, `beat`, `bar`,
  `barBeat`, `beatsPerMinute`, `beatsPerBar`, `beatUnit`, `speed`) is forged into
  every atom input port that `atom:supports time:Position`, so tempo-synced
  effects (e.g. LSP delays) follow the clock. Incoming MIDI is also forwarded to
  plugin atom inputs that support `midi:MidiEvent`, and to VST3 plugins as
  channel events (`IEventList`). VST3 tempo/transport is published through the
  process context (`tempo`, `projectTimeMusic`, `kPlaying`), so VST3 tempo-synced
  effects (e.g. ZamDelay) follow the clock too. Plugins with their own sync
  switch still need it enabled (LSP's flanger/phaser require **Tempo sync** plus
  **Time computing method = Tempo**).
- **Transport:** `0xFA` Start (reset position), `0xFB` Continue (resume), `0xFC`
  Stop (hold position), optional `0xF2` Song Position; a "locked" state with a
  timeout when clock stops.
- **Control surface:** incoming CC is matched against a per-session binding table
  (fixed, lock-free, applied on the audio thread) and drives strip fader (gain),
  pan, mute, solo, bus fader (gain), bus mute, send gain/delay, and plugin
  parameters/bypass. Absolute and relative encoders are supported, matched CCs
  are consumed (others are forwarded to plugins), and bindings are learnable and
  saved in the session.
- **UI:** the top status bar shows an always-on host-BPM readout (dimmed until
  the clock locks), and has a right-click menu to toggle counters (beats:bars
  since start, minutes:seconds, loop indicator) and open a small loop-settings
  popup (unit beats/bars, value, beats-per-bar). The engine publishes
  `{ locked, playing, bpm, beats_since_start, seconds_since_start }` at meter
  rate; counter/loop settings live in the session.
- **Channels:** each strip and bus owns an optional MIDI channel ("any", or
  1-16) set from the bindings grid; its fader/pan/mute/solo/send bindings
  listen on that channel (several strips may share one, and mixer bindings
  ignore their stored channel in favour of the owner's). Each plugin owns a
  single optional channel too: while learning a parameter, only CCs on that
  channel are captured, and the binding follows it. Plugin parameters are bound
  from the plugin editor (right-click a parameter row for CC/Learn/Clear), and
  active bindings show as `channel:CC` beside the row.
- **Out of scope:** instruments and MIDI-only plugins (would drift toward a DAW
  / sequencer); clock generation; MIDI output/feedback (future).

Designations: LV2 remains the primary format. VST3 hosting (Phase 7) is in
progress behind a format-agnostic `PluginNode` trait: discovery, add, DSP,
parameter editing, sandboxed loading and session state all work through the
vendored SDK (MIT since 3.8). CLAP was considered and set aside (thin
ecosystem); VST2 is not planned. See `docs/backlog.md`.

## Editing history and clipboard

Plugin copy/cut/paste keeps one `PluginSlot` (uri, name, params, LV2 state,
bypass) plus the chain it came from; paste appends to the chain selected by the
destination `location`. Copying captures the live control values and plugin
state, so native-UI edits are included. Duplicate strip/bus clones the model
entry, registers its ports, and re-instantiates each plugin from a captured
snapshot (external JACK `connections` are not copied; the master bus cannot be
duplicated).

Undo is snapshot-based and lives in the engine (`Controller`). Before every
mutating message the current `MixerModel` is cloned onto a bounded stack
(count and total-byte caps; window geometry is excluded). Continuous parameters
(fader/knob/plugin-param drags) coalesce over ~450 ms into one entry. On undo
the snapshot is diffed against the current model: if only values changed the
parameters are re-applied in place, otherwise (add/remove/move/duplicate/paste,
or a plugin's LV2 state changed) the graph is rebuilt like a session load and
open plugin UIs are closed. Native plugin-window edits are not individually
tracked and may be reverted when undoing past them. New/loaded sessions clear
the history.
