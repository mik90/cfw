#!/usr/bin/env bash
# Fails when the Cargo and Bazel cxx versions drift apart.
set -euo pipefail

cargo_lock="$1"
module="$2"

cargo_version() {
    local name="$1"
    awk -v name="$name" '
        $1 == "name" && $3 == "\"" name "\"" { found = 1; next }
        found && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' "$cargo_lock"
}

bazel_version="$(sed -n 's/.*bazel_dep(name = "cxx.rs", version = "\([^"]*\)").*/\1/p' "$module" | head -n 1)"

cargo_cxx="$(cargo_version cxx)"
cargo_cxx_build="$(cargo_version cxx-build)"

if [[ -z "$cargo_cxx" || -z "$cargo_cxx_build" ]]; then
    echo "could not find cxx and cxx-build versions in $cargo_lock" >&2
    exit 1
fi
if [[ -z "$bazel_version" ]]; then
    echo "could not find cxx.rs version in $module" >&2
    exit 1
fi

if [[ "$cargo_cxx" != "$bazel_version" || "$cargo_cxx_build" != "$bazel_version" ]]; then
    echo "cxx version drift:" >&2
    echo "  Cargo cxx:       $cargo_cxx" >&2
    echo "  Cargo cxx-build: $cargo_cxx_build" >&2
    echo "  Bazel cxx.rs:    $bazel_version" >&2
    echo "Keep Cargo.lock and MODULE.bazel on the same cxx version." >&2
    exit 1
fi

echo "cxx versions agree at $bazel_version"
