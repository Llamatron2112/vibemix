# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[1.0.1]: https://github.com/Llamatron2112/vibemix/releases/tag/v1.0.1
[1.0.0]: https://github.com/Llamatron2112/vibemix/releases/tag/v1.0.0
