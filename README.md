# vibemix

A modular audio mixer for JACK/PipeWire with **LV2, VST3 and CLAP** plugin
support, split into a real-time audio engine and a Qt Quick GUI that talk over a
small OSC control protocol.

## Features

- Strips and buses with input trim, pre- and post-fader insert chains,
  fader/pan/mute/solo, and per-bus sends.
- Bus inserts, mix-to-master submixes, and **external aux returns** (insert
  semantics: a pre-fader `SEND`, a `RET` trim, and the bus fader post-device).
- Per-send **alignment delay** (0–500 ms) to phase-align parallel paths.
- **Plugin hosting: LV2, VST3 and CLAP** (audio effects; MIDI passthrough;
  sidechain where the plugin exposes it).
- **Plugin browser** with a name filter, per-format/channel/sidechain/failed
  toggles, and a details pane (vendor, category, version, id, bundle).
- **Native plugin editors** (LV2 out of process; VST3/CLAP in-process X11).
- A crash-isolated **plugin health sweep**: broken plugins are shown red and
  kept bypassed instead of vanishing.
- **MIDI and keyboard bindings** for mixer buttons (mute/solo/bypass): learn a
  control change or a key combo, saved with the session.
- Sessions (TOML) with plugin state, patch info, and window geometry; File menu
  with Open/Save/Save As/Recent and an unsaved-changes prompt.
- Metering with selectable taps (strip IN/GAIN/OUT; bus IN/SEND/RET/OUT).

## Using the UI

- **Menu bar**: **Add** for strips/buses, **Options** for MIDI sync, MIDI
  bindings… and auto-fit, **File**/**Edit** for sessions and undo. Right-click
  the top bar for the transport readouts, counters and loop settings.
- **Add a plugin**: right-click a strip's insert chain → **Add plugin…**. The
  browser lists LV2/VST3/CLAP; filter by name, or toggle formats, Mono/Stereo,
  SC-only (sidechain-capable) and Failed. Click a row to add it.
- **Plugin editor**: double-click a plugin row to open its GUI, or right-click
  it for Open GUI / Parameters… / Bypass / Sidechain / Move / Copy / Remove.
- **Sidechain**: enable it in a plugin's row menu, then feed it from a
  strip/bus sidechain panel.
- **Meters**: click a meter to cycle its tap (strip IN/GAIN/OUT; bus
  IN/SEND/RET/OUT).
- **MIDI bindings…**: right-click a mute/solo cell → Learn (MIDI CC) or
  **Learn key** (press a key or combo; Esc cancels, Backspace clears).

## Build

Prerequisites are listed in [docs/prerequisites.md](docs/prerequisites.md)
(Arch packages, Fedora mapping, Qt/cxx-qt and JACK/PipeWire notes). The VST3 SDK
and CLAP headers are vendored submodules:

```bash
git submodule update --init --recursive

just build            # debug
just release          # optimized
just run-engine       # run the engine from source
just run-gui          # run the GUI from source
```

## Install

```bash
just install                      # -> /usr/local
just PREFIX=~/.local install      # custom prefix
just PREFIX=~/.local uninstall
```

Installs `vibemix-engine`, `vibemix-gui`, a `vibemix` launcher (starts the
engine for the session, then the GUI), a `.desktop` entry and an icon.

## Layout

| Path | Contents |
|---|---|
| `crates/vibemix-core` | shared model, ids, params, backend traits |
| `crates/vibemix-proto` | OSC control messages/addresses |
| `crates/vibemix-lv2` | lilv-based LV2 host (instances, worker, state, UI) |
| `crates/vibemix-vst3` | VST3 host (SDK subset shim + Rust wrapper) |
| `crates/vibemix-clap` | CLAP host (header-only shim + Rust wrapper) |
| `crates/vibemix-engine` | JACK/PipeWire engine, DSP, plugin chains, OSC control |
| `crates/vibemix-gui` | cxx-qt/QML frontend |
| `docs/` | architecture, prerequisites, backlog |
| `spikes/` | early feasibility spikes (excluded from the workspace) |

## Documentation

- [docs/architecture.md](docs/architecture.md) — signal flow, engine/GUI design,
  plugin hosting, control protocol, real-time safety rules, milestones.
- [docs/prerequisites.md](docs/prerequisites.md) — system setup and build env.
- [docs/backlog.md](docs/backlog.md) — deferred ideas and follow-ups.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
