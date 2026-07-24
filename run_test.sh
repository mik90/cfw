#!/usr/bin/env bash
set -euo pipefail

cargo check --workspace --all-targets --all-features
cargo fmt
cargo clippy --fix --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo +nightly miri test --workspace --all-features
