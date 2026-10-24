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
cargo build -p librepods-cli --no-default-features --locked
python3 tests/cli/contract.py "${CARGO_LLVM_COV_TARGET_DIR}/debug/librepods-cli" -v
cargo llvm-cov report --json --output-path "$report"
python3 scripts/check-coverage.py "$report" --core-min 90 --service-min 80
