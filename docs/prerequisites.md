# Prerequisites

Target platform: Arch Linux (KDE, PipeWire). Fedora package names are mapped at
the bottom.

## Install

```bash
sudo pacman -S --needed base-devel rust cmake ninja lld \
  qt6-base qt6-declarative \
  lv2 lilv serd sord sratom suil pipewire-jack libglvnd libx11
```

Notes:

- `base-devel` provides `gcc`/`g++`, `make` and `pkgconf`; `rust` includes
  `cargo`.
- `serd` is required at build time by the `lilv` crate (its `build.rs` probes
  `lilv-0 >= 0.24.2` and `serd-0 >= 0.30.0`).
- `qt6-declarative` ships Qt Quick Controls 2 (there is no separate
  `qt6-quickcontrols2` package on Arch) and is required by `cxx-qt-lib`'s
  `qt_full` feature.
- `lld` provides a fast linker; Qt's Cargo link step fails with the default
  `ld.bfd`.
- `clang` / `libclang` are **not** required for `cxx-qt` (a C++17 compiler is
  enough).
- Do **not** install `jack2`: it conflicts with `pipewire-jack`, which already
  ships the JACK headers, `libjack.so` and `jack.pc`.
- Optional LV2 test plugins: `sudo pacman -S calf lsp-plugins-lv2 x42-plugins
  zam-plugins`.
- Optional **GTK plugin UIs** (e.g. Calf's GTK2 GUI): `gtk3` is in the official
  repos, while `gtk2` is **AUR-only**, so Calf's GTK2 UI needs it built from
  the AUR. Nothing is linked against at build time; the engine `dlopen`s GTK
  and simply skips GTK UIs when it is missing.

## Verify

| Check | Command | Expected |
|---|---|---|
| Rust compiler | `rustc --version` | >= 1.85 |
| Cargo | `cargo --version` | matches rustc |
| C/C++ | `g++ --version` | >= C++17 |
| CMake / Ninja | `cmake --version`, `ninja --version` | present |
| Qt6 base dev | `ls /usr/lib/cmake/Qt6Core/Qt6CoreConfig.cmake` | exists |
| Qt6 QML dev | `ls /usr/lib/cmake/Qt6Qml/Qt6QmlConfig.cmake` | exists |
| Qt6 tools | `ls /usr/bin/qmake6` | exists |
| LV2 | `pkg-config --modversion lv2` | present |
| lilv / serd | `pkg-config --modversion lilv-0 serd-0` | present |
| sord / sratom / suil | `pkg-config --modversion sord-0 sratom-0 suil-0` | present |
| JACK headers | `pkg-config --modversion jack` | provided by pipewire-jack |
| JACK lib | `ldconfig -p \| grep libjack.so.0` | `/usr/lib/pipewire-0.3/jack/` |
| Server | `systemctl --user is-active pipewire wireplumber` | `active` |
| Graph tools | `pw-link -l` | lists ports |
| QML lint | `qmllint --version` | present (Arch names it `qmllint`, not `qmllint6`) |

## cxx-qt build environment

`cxx-qt-build` locates Qt via `qmake`. On Arch `qmake6` is on `PATH`, so Qt is
detected automatically; set one of the following to be explicit:

```bash
export QMAKE=/usr/bin/qmake6
# or
export QT_VERSION_MAJOR=6
```

Qt's link step needs a fast linker. Add `~/.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

## Build and install

```bash
just build                 # debug
just release               # optimized (lto)
just run-engine            # run the engine from source
just run-gui               # run the GUI from source

just install               # install to /usr/local
just PREFIX=~/.local install   # or a custom prefix
just PREFIX=~/.local uninstall
```

`just install` installs `vibemix-engine`, `vibemix-gui`, a `vibemix` launcher
(starts the engine for the session, then opens the GUI), a `.desktop` entry and
an icon.

Environment variables:

- `VIBEMIX_OSC_PORT` — control port for the engine and GUI (default `9000`).
- `VIBEMIX_CONFIG_DIR` — config directory (default `$XDG_CONFIG_HOME/vibemix`,
  i.e. `~/.config/vibemix`); holds `recent.json`, `catalog.json`,
  `plugin_health.json` and `sessions/`.
- `VIBEMIX_LV2_BLACKLIST` — optional, comma-separated plugin URIs to keep out of
  the catalog entirely (operator escape hatch; there is no built-in list).
- `VIBEMIX_DEBUG=1` (or `vibemix-engine --debug`) — verbose plugin diagnostics:
  log the smoke child's crash signal and keep its stderr.
- `vibemix-engine --probe-vst3 <bundle.vst3> <class-id>` — feed a sine through a
  VST3 plugin and print its buses, parameters, setup/process results and output
  peak, to diagnose a silent plugin without a running JACK session.
  `VIBEMIX_PROBE_PARAMS=1` also pushes every parameter to its default each block.
- `VIBEMIX_GTK` — force the GTK major version used to host plugin UIs:
  `gtk2`, `gtk3`, or unset/`auto` (pick the version that has hostable UIs). GTK2
  and GTK3 cannot coexist in one process, so only one is ever used.
- `VIBEMIX_UI_LOCAL=1` — host LV2 editors in the engine process instead of the
  out-of-process UI host (debugging/verification; may cause xruns).
- `VIBEMIX_RT_GUARD=1` — abort (debug) on any heap operation on the audio thread.

## Optional runtime dependencies (packaging)

GTK is optional: it enables hosting GTK2/GTK3 plugin UIs (e.g. Calf) and is
`dlopen`ed at runtime. Declare it as a *weak/optional* dependency so packagers
install it when available without making vibemix uninstallable without it:

- Arch (`PKGBUILD`): `optdepends=('gtk3: GTK3 plugin UIs' 'gtk2: GTK2 plugin UIs')`
- RPM (Fedora/openSUSE): `Recommends: gtk3` (optionally `gtk2`)
- Debian/Ubuntu: `Recommends: libgtk-3-0` (optionally `libgtk2.0-0`)
- Gentoo: USE flags; Flatpak/AppImage: bundle or leave to the host

## PipeWire instead of jackd

JACK is provided by PipeWire; do not install `jack-audio-connection-kit` (it conflicts with `pipewire-jack`). Clients link against `libjack.so` and appear in the PipeWire graph. Inspect with `pw-link -l`, `pw-dump`, `pw-cli info 0`.

Low-latency quantum for testing:

```bash
pw-metadata -n settings 0 clock.force-quantum 256
pw-metadata -n settings 0 clock.force-quantum 0   # reset to auto
```

## Fedora package mapping

| Arch | Fedora |
|---|---|
| `rust` (includes cargo) | `rust` + `cargo` |
| `gcc` | `gcc-c++` |
| `cmake` | `cmake` |
| `ninja` | `ninja-build` |
| `qt6-base` | `qt6-qtbase-devel` |
| `qt6-declarative` | `qt6-qtdeclarative-devel` |
| `qt6-declarative` (ships Quick Controls 2) | `qt6-qtquickcontrols2-devel` |
| `lv2` | `lv2-devel` |
| `lilv` | `lilv-devel` |
| `serd` | `serd-devel` |
| `sord` | `sord-devel` |
| `sratom` | `sratom-devel` |
| `suil` | `suil-devel` |
| `libglvnd` | `mesa-libGL-devel` |
| `libx11` | `libX11-devel` |
| `pipewire-jack` | `pipewire-jack-audio-connection-kit-devel` |
| `lld` | `lld` |

Arch's `pipewire-jack` ships JACK headers, `libjack.so` and `jack.pc` in one
package. Do not also install `jack2`. Fedora's
`pipewire-jack-audio-connection-kit-devel` plays the same role; do not install
`jack-audio-connection-kit`.

## Submodules

`vendor/vst3sdk` provides the Steinberg VST3 SDK used by `vibemix-vst3`. It is
**MIT-licensed** (since SDK 3.8; we vendor 3.8.1), so it does not change the
project license. Fetch it and its nested submodules with:

```
git submodule update --init --recursive vendor/vst3sdk
```

The crate's `build.rs` compiles only the hosting subset it needs (no CMake) and
builds a small test effect for its tests.
