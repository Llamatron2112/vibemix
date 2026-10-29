# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Loop recorder** (`Options → Recorder…`): capture a bar-aligned loop from
  the MIDI clock, summing any strips/buses at selectable taps. Takes get
  automatic JACK/plugin latency compensation, optional transient alignment and
  a circular manual nudge, and can be saved as 16-bit, 24-bit or 32-bit float
  WAV. `Options → Loops…` is a separate launcher window with quantized
  play/stop, gain, mute and an editor. Loops are stored in the session and
  under `VIBEMIX_CONFIG_DIR/loops/`.

## [1.0.2] - 2026-10-22

### Added

- In-app **patchbay** (`Options → Patchbay…`): a source × sink matrix over the
  JACK port graph, with a device picker per axis, hover crosshair, cell
  tooltips and level squares on vibemix ports. Connections are queried and
  edited over the control protocol; each cell is one source → sink connection.
- **MIDI input** picker (`Options → MIDI input…`) for the single `midi_in`
  port.
- Mixer **layout**: strips and buses live in side-by-side, independently
  scrolling panes that wrap their cards into rows. `Options → Layout…` chooses
  Auto or a fixed 1–12 cards per row, saved with the session.
- Strip/bus capacity defaults raised to **64 / 32** (`--strips`/`--buses`
  still override); Add is disabled at the limit and the panes show usage.
- Auto-fit now sizes the window tightly to the actual content and preserves the
  wrap that was active when enabled.

### Changed

- Bus cards match the strip card height so the panes align.
- Drag-to-reorder works across wrapped rows (2D drop index).
- Removed the experimental L/R pairing helpers from the patchbay.

### Packaging

- Added a manually triggered openSUSE Tumbleweed RPM test build (snapshot
  artifact; OBS is the planned release channel for Tumbleweed).

## [1.0.1] - 2026-10-11

### Fixed

- Release packages are now built and verified **per distro release**, against
  that release's Qt: `.deb` for Debian 12 and 13, `.rpm` for Fedora 43 and 44.
  The previous single build used Qt *private* API through the ahead-of-time QML
  compiler (`qmlcachegen`), which Qt only keeps for the exact Qt minor it was
  built against — so the Fedora 43 package failed to load on Fedora 44
  (`Qt_6.10_PRIVATE_API` not found).
- Package metadata pins the Qt series with both a floor and a ceiling, so the
  package manager refuses to install a build alongside an incompatible Qt.

### Removed

- The standalone binary tarball: it was equally tied to a single Qt and added no
  portability.

## [1.0.0] - 2026-10-11

First release.

### Added

- Real-time JACK/PipeWire engine and a Qt Quick GUI, split into two processes
  that talk over a small OSC control protocol.
- Strips and buses: input trim, pre- and post-fader insert chains,
  fader/pan/mute/solo, per-bus sends, external aux returns, and per-send
  alignment delay (0–500 ms).
- Plugin hosting for **LV2** (`vibemix-lv2`), **VST3** (`vibemix-vst3`) and
  **CLAP** (`vibemix-clap`) behind a format-agnostic `PluginNode` trait, with
  native editors (LV2 out of process; VST3/CLAP embedded in X11) and sidechain
  routing from per-strip sends.
- Plugin browser with a name filter, per-format/channel/sidechain/failed
  toggles, and a details pane (vendor, category, version, id, bundle).
- A crash-isolated plugin health sweep with persisted verdicts; broken plugins
  are shown red and kept bypassed instead of vanishing.
- MIDI clock-slave sync (host tempo/transport for LV2 and VST3) and a learnable
  MIDI + keyboard binding surface for mixer buttons.
- Sessions (TOML) with plugin state, patch info and window geometry, plus
  snapshot-based undo/redo and a single-plugin clipboard.
- Metering with selectable taps (strip IN/GAIN/OUT; bus IN/SEND/RET/OUT).
- Packaging: an Arch `PKGBUILD`, and CI-built `.deb` (Debian 12), `.rpm`
  (Fedora 43) and binary tarballs attached to each release with SHA-256
  checksums.

[1.0.2]: https://github.com/Llamatron2112/vibemix/releases/tag/v1.0.2
[1.0.1]: https://github.com/Llamatron2112/vibemix/releases/tag/v1.0.1
[1.0.0]: https://github.com/Llamatron2112/vibemix/releases/tag/v1.0.0
