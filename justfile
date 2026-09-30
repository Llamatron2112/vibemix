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

# Run the engine under VIBEMIX_RT_GUARD=1 for a few seconds: it aborts
# (non-zero) if the audio thread allocates or frees. Needs PipeWire's pw-jack.
rt-guard:
    #!/usr/bin/env bash
    set -euo pipefail
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    set +e
    timeout 8 env VIBEMIX_CONFIG_DIR="$dir" VIBEMIX_RT_GUARD=1 \
        pw-jack ./target/debug/vibemix-engine --osc-port 9931 >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 124 ]; then
        echo "rt-guard: engine exited early with $status (RT heap op or start failure)" >&2
        exit 1
    fi
    echo "rt-guard: ok"

release:
    cargo build --release --workspace

# Install binaries, launcher, desktop entry and icon.
# Depends on `release` so `target/release/*` is always fresh.
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
