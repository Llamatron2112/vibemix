# vibemix

A modular LV2 audio mixer for JACK/PipeWire, split into a real-time audio engine
and a Qt Quick GUI that talk over a small OSC control protocol.

## Features

- Strips and buses with input trim, pre- and post-fader LV2 insert chains,
  fader/pan/mute/solo, and per-bus sends.
- Bus inserts, mix-to-master submixes, and **external aux returns** (insert
  semantics: a pre-fader `SEND`, a `RET` trim, and the bus fader post-device).
- Per-send **alignment delay** (0–500 ms) to phase-align parallel paths.
- LV2 plugin management: modal picker with name filter, on-demand rescan,
  catalog persisted to `catalog.json`, and missing plugins kept + bypassed +
  shown red.
- Sessions (TOML) with plugin state, patch info, and window geometry; File menu
  with Open/Save/Save As/Recent and an unsaved-changes prompt.
- Metering with selectable taps (strip IN/GAIN/OUT; bus IN/SEND/RET/OUT).

## Build

Prerequisites are listed in [docs/prerequisites.md](docs/prerequisites.md)
(Arch packages, Fedora mapping, Qt/cxx-qt and JACK/PipeWire notes).

```bash
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
| `crates/vibemix-engine` | JACK/PipeWire engine, DSP, LV2 chains, OSC control |
| `crates/vibemix-gui` | cxx-qt/QML frontend |
| `docs/` | architecture and prerequisites |
| `spikes/` | early feasibility spikes (excluded from the workspace) |

## Documentation

- [docs/architecture.md](docs/architecture.md) — signal flow, engine/GUI design,
  control protocol, real-time safety rules, milestones.
- [docs/prerequisites.md](docs/prerequisites.md) — system setup and build env.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
