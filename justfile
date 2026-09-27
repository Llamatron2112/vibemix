set shell := ["bash", "-cu"]

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

spike-0a:
    cargo run --manifest-path spikes/0a-jack-passthrough/Cargo.toml

spike-0b:
    cargo run --manifest-path spikes/0b-lv2-host/Cargo.toml

spike-0c:
    cargo run --manifest-path spikes/0c-cxx-qt-qml/Cargo.toml
