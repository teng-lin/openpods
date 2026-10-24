#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Include actual CLI subprocess execution as well as Rust tests in the report.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
report="${1:-target/coverage.json}"
cargo llvm-cov clean --workspace
# cargo-llvm-cov emits shell-quoted exports for the current workspace/toolchain.
eval "$(cargo llvm-cov show-env --sh)"
cargo test --workspace --no-default-features --locked
cargo test --workspace --no-default-features --features librepods-cli/ble --locked
cargo build -p librepods-cli --no-default-features --features librepods-cli/ble --locked
python3 tests/cli/contract.py "${CARGO_LLVM_COV_TARGET_DIR}/debug/librepods-cli" --ble-build -v
cargo build -p librepods-cli --no-default-features --locked
python3 tests/cli/contract.py "${CARGO_LLVM_COV_TARGET_DIR}/debug/librepods-cli" -v
cargo llvm-cov report --json --output-path "$report"
cargo llvm-cov report --lcov --output-path "${report%.json}.lcov"
# Build this standalone analysis utility without inheriting workspace coverage.
CARGO_TARGET_DIR="${CARGO_LLVM_COV_TARGET_DIR}/span-tool" RUSTC_WRAPPER= \
  cargo run --manifest-path tools/coverage-spans/Cargo.toml --locked -- "$report" > "${report%.json}-test-spans.json"
python3 scripts/check-coverage.py "$report" --lcov "${report%.json}.lcov" \
  --test-spans "${report%.json}-test-spans.json" --core-min 90 --service-min 80
