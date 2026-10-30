set shell := ["bash", "-cu"]

PREFIX := "/usr/local"

default:
    @just --list

check:
    cargo check --workspace

build:
    cargo build --workspace

test:
    cargo test --workspace

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets -- -D warnings

run-engine:
    cargo run -p vibemix-engine

run-gui:
    cargo run -p vibemix-gui

# Smoke-test every hostable plugin in isolation and print JSON verdicts.
scan-plugins:
    cargo run -p vibemix-engine -- --scan-plugins

# Run the engine under VIBEMIX_RT_GUARD=1 while scripts/rt-guard-drive.py adds,
# removes and edits strips, buses and plugins over OSC. The engine aborts
# (non-zero) if the audio thread allocates or frees. Needs PipeWire's pw-jack.
rt-guard:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q -p vibemix-engine
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    set +e
    timeout 240 env VIBEMIX_CONFIG_DIR="$dir" VIBEMIX_RT_GUARD=1 \
        pw-jack ./target/debug/vibemix-engine --socket "$dir/engine.sock" --auto-connect false \
        --client-name vibemix-rtguard >/dev/null 2>&1 &
    engine=$!
    sleep 6
    python3 scripts/rt-guard-drive.py "$dir/engine.sock" "$dir"
    sleep 2
    if ! kill -0 "$engine" 2>/dev/null; then
        wait "$engine"; status=$?
        echo "rt-guard: engine exited early with $status (RT heap op or start failure)" >&2
        exit 1
    fi
    kill "$engine" 2>/dev/null || true
    wait "$engine" 2>/dev/null || true
    set -e
    echo "rt-guard: ok"

# Counts malloc/free calls on the audio thread (LD_PRELOAD, scripts/rtcount.c)
# with one LV2 (MIDI-capable if available), VST3 and CLAP plugin loaded and a
# 120 BPM MIDI clock plus notes playing into the engine. Unlike rt-guard this also sees C/C++
# allocations inside the plugin shims. Fails above a threshold that separates a
# one-off warm-up (a few hundred) from a per-block allocation (hundreds of
# thousands over the run). Needs gcc and pw-jack.
rt-count:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q -p vibemix-engine --bins --examples
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    gcc -shared -fPIC -O1 -o "$dir/rtcount.so" scripts/rtcount.c -lpthread
    LD_PRELOAD="$dir/rtcount.so" VIBEMIX_CONFIG_DIR="$dir" \
        pw-jack ./target/debug/vibemix-engine --socket "$dir/engine.sock" --auto-connect false \
        --client-name vibemix-rtcount \
        >/dev/null 2>"$dir/engine.err" &
    engine=$!
    sleep 6
    ./target/debug/examples/midi_clock vibemix-rtcount >/dev/null 2>&1 &
    clock=$!
    python3 scripts/rt-count-drive.py "$dir/engine.sock" "$dir" 15
    kill "$clock" 2>/dev/null || true
    kill -TERM "$engine"
    wait "$engine" || true
    line="$(grep RTCOUNT "$dir/engine.err" | tail -1)"
    echo "rt-count: $line"
    mallocs="$(sed -n 's/.*mallocs=\([0-9]*\).*/\1/p' <<<"$line")"
    if [ -z "$mallocs" ] || [ "$mallocs" -gt 5000 ]; then
        echo "rt-count: too many audio-thread allocations" >&2
        exit 1
    fi
    echo "rt-count: ok"

# Measures GUI CPU per thread at 1, 16 and 64 strips (meters running, and during
# a fader drag) with the GUI on Qt's offscreen platform and nothing audible.
# Pass a budget, e.g. `just gui-load 25`, to fail above that main-thread percent.
gui-load budget="":
    cargo build --release --workspace --bins --examples
    python3 scripts/gui-load.py {{ if budget == "" { "" } else { "--budget " + budget } }}

# Visual regression check for the GUI, headless (see scripts/gui-shot.py): run
# `just gui-shot baseline` before a GUI change and `just gui-shot check` after.
# The scenario is `static` (the fixed session) or `edited` (live edits on top).
gui-shot mode="check" scenario="static" dir="/tmp/vibemix-gui-shots":
    cargo build --release --workspace --bins --examples
    python3 scripts/gui-shot.py {{mode}} {{dir}} {{scenario}}

# Desktop-behaviour checks on a private headless compositor (nothing appears on
# the real screen): editors must close with the engine, and auto-fit must resize
# the window once. Needs kwin_wayland and python-xlib.
gui-desktop *which:
    cargo build --release --workspace --bins
    python3 scripts/gui-desktop-check.py {{which}}

# Mutation fuzzing of every parser that takes outside data: the control
# protocol, the editor-host IPC, LV2 atoms, saved plugin state, and session
# files. The same tests run briefly in `just test`; this runs them for millions
# of iterations in release mode (set ITERS to change that).
fuzz iters="1000000":
    VIBEMIX_FUZZ_ITERS={{iters}} cargo test --release -p vibemix-proto --test fuzz
    VIBEMIX_FUZZ_ITERS={{iters}} cargo test --release -p vibemix-lv2 fuzz_tests
    VIBEMIX_FUZZ_ITERS={{iters}} cargo test --release -p vibemix-vst3 --test blob_fuzz
    VIBEMIX_FUZZ_ITERS={{iters}} cargo test --release -p vibemix-clap --test blob_fuzz
    VIBEMIX_FUZZ_ITERS=$(( {{iters}} / 10 )) cargo test --release -p vibemix-engine fuzz_tests

# Memory-safety run under AddressSanitizer + UBSan. The C/C++ shims (plugin
# hosts, VST3 SDK sources, editor host) are instrumented; the Rust code is not
# (that needs nightly), but its allocations go through the sanitizer's. Runs the
# test suites, the engine with plugins of every format under load and a hostile
# message stream, the isolated smoke test of every installed plugin, and (when a
# compositor is available) plugin editors opening and closing.
#
# The runtime is PRELOADED rather than linked: it must load first for its
# allocator to be used, and without that heap errors go unnoticed.
asan:
    #!/usr/bin/env bash
    set -euo pipefail
    export CARGO_TARGET_DIR=target/asan
    export CFLAGS="-fsanitize=address,undefined -fno-omit-frame-pointer -g -O1"
    export CXXFLAGS="$CFLAGS"
    export RUSTFLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-Wl,--no-as-needed -C link-arg=-lasan -C link-arg=-lubsan"
    target=x86_64-unknown-linux-gnu
    engine="$PWD/target/asan/$target/debug/vibemix-engine"
    runtime="$(gcc -print-file-name=libasan.so)"
    export ASAN_OPTIONS=detect_leaks=0 UBSAN_OPTIONS=print_stacktrace=1
    cargo build -p vibemix-engine --target "$target"
    echo "== test suites"
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env LD_PRELOAD=$runtime" \
        cargo test --target "$target" -p vibemix-proto -p vibemix-lv2 -p vibemix-vst3 \
        -p vibemix-clap -p vibemix-core -p vibemix-engine
    echo "== engine under load, hostile messages, then shutdown"
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    cp "$HOME/.config/vibemix/catalog.json" "$HOME/.config/vibemix/plugin_health.json" "$dir/" 2>/dev/null || true
    VIBEMIX_CONFIG_DIR="$dir" LD_PRELOAD="$runtime" pw-jack "$engine" \
        --socket "$dir/engine.sock" --auto-connect false --client-name vibemix-asan \
        >"$dir/engine.log" 2>&1 &
    pid=$!
    for _ in $(seq 120); do grep -q "control ready" "$dir/engine.log" && break; sleep 0.5; done
    python3 scripts/rt-guard-drive.py "$dir/engine.sock" "$dir"
    python3 scripts/fuzz-control.py "$dir/engine.sock" 8000 1
    kill -TERM "$pid"; wait "$pid" || true
    if grep -aE "ERROR: AddressSanitizer|runtime error:" "$dir/engine.log"; then
        echo "asan: the sanitizer reported errors (above)" >&2; exit 1
    fi
    echo "== isolated smoke test of every installed plugin"
    python3 scripts/asan-smoke.py "$engine"
    if command -v kwin_wayland >/dev/null; then
        echo "== plugin editors opening and closing"
        VIBEMIX_ASAN=1 python3 scripts/gui-desktop-check.py shutdown
    fi
    echo "asan: ok"

release:
    cargo build --release --workspace

# Install binaries, launcher, desktop entry and icon.
# Depends on `release` so `target/release/*` is always fresh.
# Build and install a pacman package from the working tree. It provides and
# conflicts with vibemix/vibemix-git, so it replaces either package and keeps a
# single install under /usr (no ~/.local copy).
dev-package:
    cd packaging/arch/vibemix-dev && makepkg -sf

dev-install:
    cd packaging/arch/vibemix-dev && makepkg -sfi

install: release
    install -Dm755 target/release/vibemix-engine {{PREFIX}}/bin/vibemix-engine
    install -Dm755 target/release/vibemix-gui {{PREFIX}}/bin/vibemix-gui
    install -Dm755 packaging/vibemix {{PREFIX}}/bin/vibemix
    install -Dm644 packaging/vibemix.desktop {{PREFIX}}/share/applications/vibemix.desktop
    install -Dm644 packaging/vibemix.svg {{PREFIX}}/share/icons/hicolor/scalable/apps/vibemix.svg

uninstall:
    rm -f {{PREFIX}}/bin/vibemix-engine {{PREFIX}}/bin/vibemix-gui {{PREFIX}}/bin/vibemix
    rm -f {{PREFIX}}/share/applications/vibemix.desktop
    rm -f {{PREFIX}}/share/icons/hicolor/scalable/apps/vibemix.svg

spike-0a:
    cargo run --manifest-path spikes/0a-jack-passthrough/Cargo.toml

spike-0b:
    cargo run --manifest-path spikes/0b-lv2-host/Cargo.toml

spike-0c:
    cargo run --manifest-path spikes/0c-cxx-qt-qml/Cargo.toml
