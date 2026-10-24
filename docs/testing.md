# Test and release evidence

Rust 1.99.1 is pinned because it is the toolchain used for the recorded local
baseline. This is the supported compiler baseline; no lower MSRV is claimed.
`Cargo.lock` pins dependencies. Linux and Windows builds run separately, with
minimal/default and BLE feature variants. Core also receives an Android compile
check. Native Windows linking is verified on Windows CI, not by Linux type checks.

## Local checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
cargo test --workspace --no-default-features --locked
cargo test --workspace --no-default-features --features librepods-cli/ble --locked
python3 -m unittest discover -s tests/packaging -v
node tests/compat/model.mjs
node tests/compat/model.mjs --fixtures-dir target/compat-fixtures
```

Ordinary automated tests must use private buses, synthetic peers and temporary
directories. They must not change the real user's playback, profiles, default
input/output, pairings or service selection. Hardware tests are invoked separately.
The synthetic fixture contract checks are intentionally separate from fixtures
produced by the actual Rust serializer. Missing generated fixtures fail the
integration check.

## Coverage

```bash
cargo install cargo-llvm-cov --version 0.9.1 --locked
cargo llvm-cov --workspace --no-default-features --locked --json --output-path target/coverage.json
python3 scripts/check-coverage.py target/coverage.json
```

The checker counts uncovered production source in core, CLI, Linux adapters and
daemon. It excludes generated and non-host platform code by path, not arbitrary
functions. The release gate is 90% core and 80% service line coverage. Keep branch
and changed-line evidence where available; a line percentage cannot establish
that race, recovery and error transitions work.

Measured baseline at `399e7759053d4b0dfe1d37813b0ca9d0bde7af5c`, Linux,
Rust 1.99.1, `cargo-llvm-cov` 0.9.1, existing workspace tests:

| Group | Covered / measured lines | Coverage |
| --- | --- | --- |
| Core | 801 / 1056 | 75.85% |
| CLI and Linux adapters | 141 / 1355 | 10.41% |

These figures are a failing starting point, not release readiness. They include
all measured source and do not substitute the earlier physical ANC tests for
automated coverage. Use `--report-only` on the checker only to record a baseline,
never in a release gate.

## Fuzzing and interpretation

```bash
cargo install cargo-fuzz --version 0.13.2 --locked
cargo +nightly fuzz run aacp -- -max_total_time=120 -max_len=16385
cargo +nightly fuzz run ble -- -max_total_time=120 -max_len=16385
cargo +nightly fuzz run ipc -- -max_total_time=120 -max_len=16385
cargo +nightly fuzz run config -- -max_total_time=120 -max_len=16385
cargo +nightly miri test -p librepods-core --no-default-features --locked
```

Fuzz targets exercise production parsers. Keep the smallest reproducer for every
failure and add its expected behavior as a normal regression test. Scheduled
fuzzing is bounded, preserves crash artifacts, and is not exhaustive proof.
Nightly-only runs are distinct from pinned stable build compatibility.

## Manual release record

Use [release-evidence.md](release-evidence.md) to record hardware, firmware,
transport adapter, exact commit, commands, results and restoration. A blank or
pending item is incomplete. The 24-hour no-BLE soak is an elapsed-time requirement;
running a shorter process or merely adding a script does not satisfy it.

## CLI process contract

After `cargo build -p librepods-cli --locked`, run
`python3 tests/cli/contract.py target/debug/librepods-cli -v`. This launches the
actual executable with isolated HOME and XDG directories and local Unix peers.
It verifies typed ANC, conversational awareness, one-bud, ear policy and audio
commands; structured failures; response correlation; and refusal to compete with
an existing owner. It never opens a Bluetooth session or changes desktop state.
The runtime integration tests separately exercise the actual daemon actor and
protocol reducer using a fake transport, rather than replacing them with peers.

For release coverage, use `scripts/coverage.sh target/coverage.json`. It clears
old counters, instruments both minimal and optional-BLE Rust tests and the actual CLI executable, runs the
isolated CLI process suite, and enforces the same complete-source thresholds.
A plain `cargo llvm-cov` test invocation misses subprocess-only CLI paths.
