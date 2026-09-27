# Prerequisites

Target platform: Fedora 44 (KDE, PipeWire). Arch package names are mapped at the bottom.

## Install

```bash
sudo dnf install cargo cmake ninja-build gcc-c++ \
  qt6-qtbase-devel qt6-qtdeclarative-devel qt6-qtquickcontrols2-devel \
  lv2-devel lilv-devel serd-devel sord-devel sratom-devel suil-devel \
  pipewire-jack-audio-connection-kit-devel lld
```

Notes:

- `serd-devel` is required at build time by the `lilv` crate (its `build.rs` probes `lilv-0 >= 0.24.2` and `serd-0 >= 0.30.0`).
- `qt6-qtquickcontrols2-devel` is required by `cxx-qt-lib`'s `qt_full` feature.
- `lld` provides a fast linker; Qt's Cargo link step fails with the default `ld.bfd`.
- `clang` / `libclang` are **not** required for `cxx-qt` (a C++17 compiler is enough).
- Optional LV2 test plugins: `sudo dnf install calf lsp-plugins-lv2 x42-plugins zam-plugins`.

## Verify

| Check | Command | Expected |
|---|---|---|
| Rust compiler | `rustc --version` | >= 1.85 |
| Cargo | `cargo --version` | matches rustc |
| C/C++ | `g++ --version` | >= C++17 |
| CMake / Ninja | `cmake --version`, `ninja --version` | present |
| Qt6 base dev | `ls /usr/lib64/cmake/Qt6Core/Qt6CoreConfig.cmake` | exists |
| Qt6 QML dev | `ls /usr/lib64/cmake/Qt6Qml/Qt6QmlConfig.cmake` | exists |
| Qt6 tools | `ls /usr/lib64/qt6/bin/qmake6` | exists |
| LV2 | `pkg-config --modversion lv2` | present |
| lilv / serd | `pkg-config --modversion lilv-0 serd-0` | present |
| sord / sratom / suil | `pkg-config --modversion sord-0 sratom-0 suil-0` | present |
| JACK headers | `pkg-config --modversion jack` | provided by pipewire-jack |
| JACK lib | `ldconfig -p \| grep libjack.so.0` | `/usr/lib64/pipewire-0.3/jack/` |
| Server | `systemctl --user is-active pipewire wireplumber` | `active` |
| Graph tools | `pw-link -l` | lists ports |

## cxx-qt build environment

`cxx-qt-build` locates Qt via `qmake`. Fedora ships it as `qmake6`, so set one of:

```bash
export QMAKE=/usr/lib64/qt6/bin/qmake6
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
  i.e. `~/.config/vibemix`); holds `recent.json`, `catalog.json` and `sessions/`.
- `VIBEMIX_LV2_BLACKLIST` — comma-separated plugin URIs to refuse to load.
- `VIBEMIX_RT_GUARD=1` — abort (debug) on any heap operation on the audio thread.

## PipeWire instead of jackd

JACK is provided by PipeWire; do not install `jack-audio-connection-kit` (it conflicts with `pipewire-jack`). Clients link against `libjack.so` and appear in the PipeWire graph. Inspect with `pw-link -l`, `pw-dump`, `pw-cli info 0`.

Low-latency quantum for testing:

```bash
pw-metadata -n settings 0 clock.force-quantum 256
pw-metadata -n settings 0 clock.force-quantum 0   # reset to auto
```

## Arch package mapping

| Fedora | Arch |
|---|---|
| `rust` + `cargo` | `rust` (includes cargo) |
| `gcc-c++` | `gcc` |
| `cmake` | `cmake` |
| `ninja-build` | `ninja` |
| `qt6-qtbase-devel` | `qt6-base` |
| `qt6-qtdeclarative-devel` | `qt6-declarative` |
| `qt6-qtquickcontrols2-devel` | `qt6-quickcontrols2` |
| `lv2-devel` | `lv2` |
| `lilv-devel` | `lilv` |
| `serd-devel` | `serd` |
| `sord-devel` | `sord` |
| `sratom-devel` | `sratom` |
| `suil-devel` | `suil` |
| `pipewire-jack-audio-connection-kit-devel` | `pipewire-jack` |
| `lld` | `lld` |

Arch's `pipewire-jack` ships JACK headers, `libjack.so` and `jack.pc` in one package. Do not also install `jack2`.
