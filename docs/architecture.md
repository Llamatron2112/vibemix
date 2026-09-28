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

Each strip has a dB input trim (`-20..+24 dB`, unity at 0) applied before the
plugin chain, so a low input can be brought up to a plugin's operating level (and
hot inputs attenuated). All levels are measured as **peak dBFS** (`20·log10`);
the strip has three meter taps — `in` (raw input), `gain` (post-trim, pre-plugin)
and `out` (post-fader/pan) — and the GUI can select which one each strip's meter
shows. The numeric readout and the meter's dB scale use `-60..+6 dBFS`.

Each strip has one post-fader send per bus (`strip.sends`), exposed as a slider
per bus in the GUI; each send also carries an alignment delay (`delay_ms`,
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
per-bus insert chain from the session and at runtime. Confirmed working against
the installed plugin set (77 of 210 stereo plugins instantiate and run).

Plugins are discovered by scanning the standard LV2 directories (`lilv`'s
`World::load_all`), keeping the hostable mono/stereo ones; the list is persisted
to `$XDG_CONFIG_HOME/vibemix/catalog.json` (full plugin metadata) on startup and
after each scan. The GUI's plugin picker (a modal window with a name filter) can
trigger a rescan (`/vibemix/plugins/rescan`), which builds a fresh `lilv` world —
the previous worlds are kept alive so instances/UI windows created from them stay
valid — re-discovers, rewrites the catalog, and rebuilds the chains. A plugin
that cannot be instantiated (uninstalled or blacklisted) is kept in the session,
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

Native plugin UIs: the engine hosts X11-native UIs in X11 windows via `suil`
(they appear through XWayland and are composited as normal windows). The engine
owns the windows because LV2 UIs need same-process instance access, and pumps
them in its control loop. Open/close from the GUI "UI" button
(`/vibemix/plugin/ui` and `/vibemix/plugin/ui/close`). Only X11 UIs are
hostable (215 plugins incl. LSP/x42); Gtk UIs (Calf) are not, and the UIs are
separate floating windows (no embedding into the QML window).

The host provides `ui:resize`, `ui:showInterface` (with the retained
`LV2UI_Resize` kept alive for the UI), and `options:options` carrying
`ui:updateRate` and `ui:scaleFactor`. It pumps every UI — including its
`ui:idleInterface` — each control-loop iteration, so animated UIs (including
LSP's OpenGL UIs) update.

Each plugin window is created with `WM_CLASS=vibemix-gui` (matching
`StartupWMClass` in `packaging/vibemix.desktop`), `_NET_WM_PID`, and
`_NET_WM_NAME` set to the plugin's name, so desktop environments group it under
the vibemix application/icon rather than a generic one.

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

Because UIs must run in-process, opening one is still gated by a short
subprocess smoke test as a safety net: a UI that crashes or hangs is refused
rather than taking the engine down. The smoke test runs headless (the window is
never mapped) and also exercises the output-forwarding path, so it produces no
visible flash.

Not supported: Gtk/Qt plugin UIs. meters.lv2 EBU R128 stays blacklisted (it
aborts on its own internal buffer).

This hosts ~209 mono/stereo plugins (LSP, Calf, x42 convolvers, meters.lv2, …).

Safety: arbitrary third-party plugins can crash the process, so before a plugin
is added the engine re-execs itself with `--smoke <uri>`, which instantiates it
offline and runs a few blocks; if the child crashes or times out the add is
refused (result cached per URI). Discovery also refuses port shapes we cannot
fully connect, and a small blacklist (overridable with
`VIBEMIX_LV2_BLACKLIST=uri1,uri2`) short-circuits known crashers (Calf Reverse
Delay, meters.lv2 EBU R128). `cargo run -p vibemix-lv2 --example probe`
smoke-tests discovery/instantiate; `--example scan_silence` checks every
hostable plugin passes audio with its defaults; `--example run_plugin <uri>`
runs one plugin offline.

## Control protocol

OSC over localhost UDP. See `vibemix-proto::address`:

| Address | Args |
|---|---|
| `/vibemix/strip/gain` | `int strip, float gain` |
| `/vibemix/strip/mute` | `int strip, int mute` |
| `/vibemix/strip/solo` | `int strip, int solo` |
| `/vibemix/strip/pan` | `int strip, float pan` |
| `/vibemix/strip/input_gain` | `int strip, float db` (input trim, `-20..+24 dB`) |
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
| `/vibemix/plugin/add` | `int location, int target, string uri` |
| `/vibemix/plugin/remove` | `int location, int target, int slot` |
| `/vibemix/plugin/move` | `int location, int target, int from, int to` |
| `/vibemix/plugin/param` | `int location, int target, int slot, int param, float value` |
| `/vibemix/plugin/bypass` | `int location, int target, int slot, int bypassed` |
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
offer Copy/Cut/Paste (labelled with the clipboard contents), and strip/bus
context menus offer Duplicate.

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
- **Unscheduled** - a native PipeWire `pw_filter` backend (finalizing the
  `AudioBackend` trait). No arbitrary module graph / port matrix is planned; the
  strip/bus/preset mixer topology is the design.

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
