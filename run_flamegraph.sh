#!/usr/bin/env bash
set -euo pipefail

PROFILE_BIN="live"
OUTPUT_DIR="$PWD/tmp"
STARTUP_DELAY_MS=3000

mkdir -p $OUTPUT_DIR

RUSTFLAGS="-Clink-arg=-Wl,--no-rosegment" \
    cargo flamegraph --bin=$PROFILE_BIN --profile profiling \
     -c "record -D $STARTUP_DELAY_MS -F 997 --call-graph dwarf,64000 -g -o $OUTPUT_DIR/perf.data" \
     -o $OUTPUT_DIR/flamegraph.svg