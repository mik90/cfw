#!/usr/bin/env bash
set -euo pipefail

export RUSTFLAGS="-D warnings"

# Fast path by default: check + fmt + clippy + nextest. Miri is opt-in since a
# full run takes minutes. `./run_test.sh --miri [nextest filter...]` runs the
# miri test suite; slow tests are excluded automatically by nextest's
# `default-miri` profile (see .config/nextest.toml). Pass a nextest filter or
# --ignore-default-filter to override.
MIRI=0
ARGS=()
for arg in "$@"; do
  case "$arg" in
    --miri) MIRI=1 ;;
    *) ARGS+=("$arg") ;;
  esac
done

./lint.sh
cargo nextest run --all-features

if [ "$MIRI" -eq 1 ]; then
  # Default subset: the crates whose logic we care about, with the
  # `default-miri` profile (see .config/nextest.toml) excluding slow tests.
  # Pass a nextest filter to override, e.g. `./run_test.sh --miri -E 'test(x)'`.
  if [ "${#ARGS[@]}" -eq 0 ]; then
    cargo +nightly miri nextest run --all-features \
      -p exact_replay_executor -p task -p logging -p test_tasks
  else
    cargo +nightly miri nextest run --all-features "${ARGS[@]}"
  fi
fi
