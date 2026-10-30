# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Zooming the loop editor no longer drops back to the whole-loop envelope.**
  The editor keeps the last window's samples and envelope, remaps them into the
  new view, and only requests a new resolution after the gesture settles.
  Envelopes are now returned alongside raw samples, so zooming out of raw mode
  keeps the current window's detail.
- **Nudging a playing loop no longer stops it.** The rotation moved from the
  audio buffer to a playback offset; nudge, snap-transient and seam-snap update
  that offset in place, keeping the position and playback state.
- **Mute/fader smoothing** was applied once per audio block, so mute took
  seconds to fully silence and steps were audible at large buffer sizes. Gains,
  pan and send delays now use time-based smoothing with per-sample ramps, so
  behaviour no longer depends on the period; mute reaches exact silence.
- **The control socket is now a Unix socket in a private directory** instead of
  a localhost UDP port that every local process could use. The engine listens on
  `$XDG_RUNTIME_DIR/vibemix/engine.sock` (directory `0700`, socket `0600`, both
  checked at startup), the GUI binds its own socket beside it, and the
  filesystem permissions decide who may talk to the engine. `--osc-port` and
  `VIBEMIX_OSC_PORT` are replaced by `--socket` and `VIBEMIX_SOCKET`. A second
  engine on the same socket now exits with status 75 and the launcher reuses the
  running one; a socket left by a crashed engine is replaced. Sends never block,
  so a stalled GUI cannot stall the engine. Measured latency and throughput match
  loopback UDP.
- **A single malformed `/vibemix/meters` message could crash the receiver**
  (`capacity overflow`, or an attempt to allocate gigabytes, from a length field
  taken straight from the message). Found by the new fuzz tests; every length is
  now validated against the arguments that arrived.
- New fuzz tests for everything that parses outside data (control protocol and
  chunk reassembly, editor-host IPC and framing, LV2 atoms, saved plugin state,
  session files, plugin-sweep output). They run briefly in `cargo test`;
  `just fuzz` runs them for millions of iterations.
- New `just asan`: the plugin-host shims and editor host are built with
  AddressSanitizer and UBSan, and the test suites, the engine under load with a
  hostile message stream, every installed plugin's smoke test (471) and plugin
  editors opening and closing all run clean. (The sanitizer runtime is
  preloaded: linked last, it silently leaves heap checking off.)
- **Closing the mixer with plugin editors open left the editor windows behind,
  white and unresponsive.** Since the engine started shutting down cleanly, its
  teardown freed the plugin instances before closing their editors and hung. It
  now closes the editors first, and a 5 s watchdog ends the process if any
  plugin teardown still hangs.
- **Auto-fit flashed another window geometry** (briefly taller or shorter) when
  a strip was added or removed. The window size now follows the content only
  once it has settled, and the column count comes from the cards that exist, so
  the window resizes once, straight to the final size.
- **The GUI no longer freezes when the session changes.** Adding or removing a
  strip, bus or plugin used to re-create every strip and bus card; with 64
  strips and 32 buses one edit tied the GUI up for 40 to 180 s (about 1 s at 16
  strips). The cards are now driven by id-only models that are updated
  incrementally, so only the cards that changed are created or destroyed: the
  same edits take about 2 s at the maximum size and 0.5 s at 16 strips. The
  rendering is unchanged (checked pixel for pixel, including after live edits).
- New `just gui-shot` recipe: a headless visual regression check for the GUI
  (`--snapshot-to=<png>` saves the window; the script builds a fixed session,
  captures it, and compares against a baseline with unstable pixels masked).
- The engine no longer re-sends the whole plugin catalog (about 220 KB) every
  second in answer to the GUI's state poll; it sends it to a new GUI and then
  only every 30 s (which still repairs lost chunks).
- New `just gui-load` recipe: starts the engine, a tone and the GUI on Qt's
  offscreen platform (nothing audible) and reports GUI CPU per thread at 1, 16
  and 64 strips, plus how long a structural edit ties the GUI up.
- **Crash recovery.** The engine autosaves the session every 60 s (when it has
  changed) and restores it on the next start if it did not shut down cleanly;
  the launcher restarts a crashed engine (up to 3 times a minute) while the GUI
  keeps running, and the GUI's connection indicator now turns red when the
  engine goes quiet instead of staying green. A bug in one control-message
  handler can no longer take the engine down (each message is panic-isolated).
- **DSP load readout** in the status bar (callback time over the period, with
  the recent peak in the tooltip), next to the xrun counter.
- Delay lines are no longer zeroed (and all their pages faulted in) on the
  audio thread the first time a send gets a delay; enabling 224 delays at once
  used to spike the callback to 95% and could xrun.
- **CLAP plugins allocated on the audio thread** (about two million `malloc`
  calls in 15 s with one plugin loaded): the host shim rebuilt its event lists
  every block, re-sent every parameter every block under a mutex, and resized
  sidechain buffers on demand. It now uses only storage reserved at open, never
  blocks, and sends parameters only when they change. Non-main CLAP buses also
  get real silence/scratch buffers instead of null channel pointers. The new
  `just rt-count` recipe measures audio-thread allocations including C/C++ code.
- `write_sidechain` for VST3/CLAP clamps the frame count to the slices and the
  shim buffers instead of trusting the caller.
- Removing a strip or bus and then adding a plugin to the recycled slot could
  allocate on the audio thread. Cleared chains now keep their capacity; `just
  rt-guard` exercises this path (now for LV2, VST3 and CLAP plugins, with
  auto-connect off).
- Faders, mutes and plugin parameters changed by MIDI CC or keyboard bindings
  were missing from saved sessions and undo snapshots.
- Opening a session no longer trusts the `bundle` path stored in it for
  VST3/CLAP plugins (it could have loaded an arbitrary library); the path now
  always comes from plugin discovery.
- Plugin state capture no longer passes audio through dry (bypassing limiters)
  while saving, and bus chains are now paused too.
- Sessions and config files are written atomically, so a crash or full disk
  cannot truncate them.
- The engine handles SIGINT/SIGTERM by shutting down cleanly (the launcher's stop
  signal used to skip all cleanup and could leave a plugin sweep running in the
  background), and a plugin sweep no longer loses its last results if its
  process exits before the output is read.
- LV2 plugins with a worker now get the `end_run` callback the worker spec
  requires.
- Loop launcher: gain, mute and stop now ramp instead of switching hard (a stop
  fades out over a few ms), and a muted loop keeps running silently so it is
  still on the bar when unmuted.
- The MIDI clock tempo no longer takes a while to recover (or reads wildly off)
  after the clock drops out and resumes; the recorder no longer logs from the
  audio thread when it discards a take.
- Toggling a plugin's bypass now crossfades between the dry and processed signal
  (about 10 ms) instead of switching hard, so it no longer clicks.
- The engine refuses to overwrite an existing file that is not a vibemix session
  (or, for loop exports, a WAV), since the local control socket is unauthenticated.
- The engine exits when the JACK/PipeWire server goes away instead of idling.
- NaN/Inf from a plugin is replaced by silence; the chunk reassembler and UI
  host framing reject absurd sizes.
- Adding or pasting a plugin no longer freezes the control loop (meters,
  faders) for up to 3 s while its crash-safety smoke test runs; the test runs in
  the background and the plugin appears when it passes. VST3/CLAP plugins are
  also no longer instantiated in the engine before that test.
- Send alignment delays: a send with zero delay is now a plain copy (no ring
  buffer work or memory touched), and a send that was off or at zero delay no
  longer replays stale audio from its delay line when it is turned back on.
- The control socket asks for a 4 MiB receive buffer on both ends so chunked
  catalog/state bursts are not dropped; the out-of-process UI socket no longer
  lives directly in `/tmp` when `XDG_RUNTIME_DIR` is unset.
- The GUI is sent meters at a steady rate and coalesced state snapshots, which
  cuts control-thread work during fader drags.

### Changed

- CI now checks `cargo fmt`; the tree was reformatted.
- Engine internals: `control.rs` (5.7k lines) is split into `control/` modules
  (`catalog`, `smoke`, `ui`, `loops`, `state`, `plugins`); behaviour is unchanged.
- Engine internals: strip and bus insert chains share one `run_chain` path, and the
  per-slot control-thread bookkeeping (state proxy, editor handle, output and UI
  mailboxes) lives in a single `SlotMeta` instead of four parallel vectors.

### Added

- The top-bar menu can hide the **DSP usage readout** independently of the
  other transport readouts; the choice is saved in the session.
- **Load the last used session on startup** from the File menu. The preference
  is global (kept in `startup.json`), the last opened/saved path is remembered,
  and crash-recovery autosaves still take priority.
- **Loop recorder** (`Options → Recorder…`): capture a bar-aligned loop from
  the MIDI clock, summing any strips/buses at selectable taps. Takes get
  automatic JACK/plugin latency compensation, optional transient alignment and
  a circular manual nudge, and can be saved as 16-bit, 24-bit or 32-bit float
  WAV. `Options → Loops…` is a separate launcher window with quantized
  play/stop, gain, mute and an editor. Saving uses a native file dialog and
  remembers the last folder in the session; exports can be normalized to
  -1 dBFS. The editor requests a
  high-resolution window for the visible range and uses a free pan/zoom view:
  drag horizontally to pan and vertically to zoom, Alt-drag to nudge (Shift =
  one sample per pixel), or type an exact value. Multiple transient markers are
  shown; Ctrl+click one to make it the loop start, or use "snap first"/"zero
  seam". Onsets now come from spectral flux (FFT) rather than amplitude, with a
  high-frequency energy strip and raw-sample drawing at close zoom. Wheel
  zoom, ± buttons, and a "samples" button make the raw view easy to reach. Each
  loop has a sensitivity slider for the detector, and pan/zoom waits 120 ms for
  the gesture to settle before requesting a new resolution, so the waveform
  moves smoothly. Loops are stored in the session and
  under `VIBEMIX_CONFIG_DIR/loops/` by default.

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
