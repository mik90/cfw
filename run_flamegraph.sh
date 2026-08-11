#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR="$PWD/tmp"
STARTUP_DELAY_MS=3000
PERF_SAMPLE_PER_SEC=997
STACK_DUMP_SIZE_BYTES=64000

PERF_COMMAND="record -D $STARTUP_DELAY_MS \
    --freq=$PERF_SAMPLE_PER_SEC \
    --call-graph dwarf,$STACK_DUMP_SIZE_BYTES \
    -o $OUTPUT_DIR/perf.data"

mkdir -p $OUTPUT_DIR

PROFILE_TARGET="profiling"

RUSTFLAGS="-Clink-arg=-Wl,--no-rosegment" \
    cargo flamegraph --bin=$PROFILE_TARGET --profile profiling \
     -c "$PERF_COMMAND" \
     -o "$OUTPUT_DIR/flamegraph.svg" \
     -- --duration-secs 15 --sets 8 --period-us 1

pushd $OUTPUT_DIR
perf script report gecko --save-only perf_gecko.json
popd
