#!/usr/bin/env sh
# SPDX-License-Identifier: GPL-3.0-or-later
# The local checks, matching CI. With --upgrade, first moves every dependency to its
# newest release (cargo-edit's cargo upgrade --incompatible, then cargo update).
# Needs cargo-audit, plus cargo-edit for --upgrade, and Node 22+ for the conformance suites.
set -eu
cd "$(dirname "$0")/.."

if [ "${1:-}" = "--upgrade" ]; then
    cargo upgrade --incompatible
    cargo update
fi

cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --lib --no-default-features -- -D warnings
cargo test --all-features
cargo audit
echo "all checks passed"
