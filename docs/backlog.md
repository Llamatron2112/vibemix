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

- **VST3 host** (in progress): VST3, not CLAP (CLAP's ecosystem is thin and
  mostly overlaps LV2; VST2 is discontinued). The SDK (v3.8.1, **MIT-licensed
  since 3.8** — no copyleft implications) is vendored as a submodule and built
  without CMake. The node trait, format-tagged model and `vibemix-vst3` host
  crate are in place; the engine discovers/adds/processes VST3 plugins, edits
  their parameters in the built-in editor, hosts their native `IPlugView`
  editor (embedded in X11), routes their sidechain input from a source bus
  (zero added latency), saves/restores their state with the session, and
  loading is sandboxed (`--smoke-vst3` per add plus the batch sweep, which
  covers VST3 too).
- Qt plugin UIs (and GTK4).

## Audio backend

- Native PipeWire `pw_filter` backend (finalize the `AudioBackend` trait).

## UI

- Smooth meters (interpolate between meter frames) and expose/tune the UI poll
  interval.
- Fuller transport/loop editor.
- Optional blocking startup: keep the "starting engine" overlay until the first
  plugin sweep finishes, with an Esc-to-skip into the mixer. Not needed today
  because the sweep is non-blocking and cancellable, but a nicer first-run
  experience for very large collections.
- Parallelize the plugin sweep (currently sequential forks) if startup testing
  ever feels slow on big collections.
