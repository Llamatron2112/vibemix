# Backlog

Deferred ideas and follow-ups that are not part of the current roadmap
(`docs/architecture.md`). Nothing here is committed to a phase.

## Plugin health

- **Detect plugin updates** (same URI, reinstalled binary) and retest them. Store
  the `.so` `mtime`+`size` in the verdict and extend "untested" to
  "no verdict or fingerprint changed". The check is cheap (one `stat` per
  plugin); the plumbing is what's missing: the `lilv` crate doesn't expose the
  library path (`lilv_plugin_get_library_uri`), so this needs a `lilv_sys` access
  or a small C shim. Bundle-level fingerprinting is coarse (one `.so` per
  bundle can hold many plugins).

## Debugging / diagnostics

- **Fuller verbose/debug mode.** `--debug` currently only logs the smoke child's
  crash signal and keeps its stderr. Extend it to a richer `--debug`/`--verbose`
  mode: structured logging of the control protocol and plugin lifecycle, a
  per-plugin backtrace capture (re-run the smoke child under `gdb`), and a
  `just debug` recipe.
- **Plugin crash reports.** Optionally write a small report (plugin URI, signal,
  backtrace, host ABI version) next to `plugin_health.json` for filing upstream.

## MIDI

- MIDI output/feedback (currently a `MidiIn` only).
- MIDI clock generation (the mixer is clock-slave only).
- Program change / bank select handling.

## Plugins / hosting

- VST3 and CLAP hosting are done (see `docs/architecture.md`). Remaining:
  Qt plugin UIs (and GTK4).

## Audio backend

- Native PipeWire `pw_filter` backend (finalize the `AudioBackend` trait).

## UI

- **GUI scalability (measured with `just gui-load`, release build, offscreen).**
  GUI main-thread CPU with meters running: 1 strip ~1%, 16 strips ~5%, 64 strips
  x 32 buses ~29%; during a fader drag ~9% at 16 strips and ~82% at 64 strips.
  A structural edit used to tie the GUI up for ~1 s at 16 strips and 40 to 180 s
  at 64 strips x 32 buses because `mix = JSON.parse(...)` re-created every card.
  **Fixed**: the strip/bus repeaters now use id-only `ListModel`s synced
  incrementally and cards look their data up by id (about 0.5 s at 16 strips and
  2 s at 64 x 32), verified pixel-identical with `just gui-shot`. Still open at
  64 x 32 (all fine at 16 strips): meters cost ~30% of a core (each
  `StereoMeter` has its own 60 ms timer and ~45 items), and a fader drag ~80%
  because every value update re-evaluates the bindings of all ~2000 send
  sliders. The JSON data path itself is cheap (about 1 ms per meter update).
  Directions: one shared meter timer and fewer items per meter (draw the scale
  once or in a Canvas), and per-entity value objects so an update only touches
  the bindings of what changed.

- Smooth meters (interpolate between meter frames) and expose/tune the UI poll
  interval.
- Fuller transport/loop editor.
- Optional blocking startup: keep the "starting engine" overlay until the first
  plugin sweep finishes, with an Esc-to-skip into the mixer. Not needed today
  because the sweep is non-blocking and cancellable, but a nicer first-run
  experience for very large collections.
- Parallelize the plugin sweep (currently sequential forks) if startup testing
  ever feels slow on big collections.
