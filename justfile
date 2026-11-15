set positional-arguments

lint:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo check --workspace --all-targets --all-features
    cargo clippy --fix --workspace --all-targets --all-features --allow-dirty --allow-staged
    cargo fmt

test *flags:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo nextest run --all-features "$@"

miri *flags:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo +nightly miri nextest run --all-features "$@"

no-features:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo check --no-default-features --all

coverage *flags:
    #!/usr/bin/env bash
    # Doctest coverage (--doctests/--doc) and branch coverage (--branch) are
    # unstable cargo-llvm-cov/rustc features that require nightly, so they are
    # not enabled here. To run them manually:
    #   cargo +nightly llvm-cov --doctests
    #   cargo +nightly llvm-cov --branch
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo llvm-cov clean --workspace
    cargo llvm-cov nextest --workspace --all-features --no-report "$@"
    cargo llvm-cov report
    cargo llvm-cov report --html
    cargo llvm-cov report --lcov --output-path target/llvm-cov/lcov.info

bazel-lint:
    #!/usr/bin/env bash
    set -euo pipefail
    buildifier -r --lint=fix .
    buildifier -r -mode=check --lint=warn .

no-std:
    cargo check -p base -p task --no-default-features --target thumbv7em-none-eabihf
