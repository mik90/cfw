lint:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo check --workspace --all-targets --all-features
    cargo fmt
    cargo clippy --fix --workspace --all-targets --all-features --allow-dirty --allow-staged

test:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo nextest run --all-features

miri:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo +nightly miri nextest run --all-features "${ARGS[@]}"

no-features:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    cargo check --no-default-features
