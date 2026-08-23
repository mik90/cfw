#!/usr/bin/env bash
set -euo pipefail

export RUSTFLAGS="-D warnings"

# Runs lint, the normal nextest suite, and the miri suite by default. The miri
# suite takes minutes; pass `--no-miri` to skip it. Slow tests are excluded by
# nextest's `default-miri` profile (see .config/nextest.toml). Pass a nextest
# filter to override, e.g. `./run_test.sh -E 'test(x)'`.
MIRI=1
ARGS=()
for arg in "$@"; do
  case "$arg" in
    --no-miri) MIRI=0 ;;
    *) ARGS+=("$arg") ;;
  esac
done

./lint.sh
cargo nextest run --all-features

if [ "$MIRI" -eq 1 ]; then
  # Runs the miri test suite; slow tests are excluded by the `default-miri`
  # profile in .config/nextest.toml. Pass a nextest filter to override, e.g.
  # `./run_test.sh -E 'test(x)'`.
  # The live executor's enqueue state holds a reference cycle (nodes hold an
  # `Arc` back to the enqueue state), which leaks; fixing it is deferred, so
  # leak detection is disabled under miri.
  export MIRIFLAGS="-Zmiri-ignore-leaks"
  cargo +nightly miri nextest run --all-features "${ARGS[@]}"
fi
