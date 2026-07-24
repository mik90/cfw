#!/usr/bin/env bash
set -euo pipefail

export RUSTFLAGS="-D warnings"

cargo check --workspace --all-targets --all-features
cargo fmt
cargo clippy --fix --workspace --all-targets --all-features --allow-dirty --allow-staged
cargo nextest run --all-features
cargo +nightly miri nextest run --all-features
