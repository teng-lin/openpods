# Local automated evidence

These results establish automated development gates at the exact snapshots below.
They do not complete physical case/suspend/audio acceptance or a 24-hour soak.
Rerun checks if application source changes after the tested checkpoint.

## Stable checkpoint

Tested commit `bc78c011f3bac3f457e739c7b63e8e186da9e7c7`, tree
`e0abada93cce0ea8860634d6cdbc91b1f6ad4bb6`: production source through `9d754bc`,
reviewed listener-fixture synchronization and the final terminal contract test.
The source tree was clean and unchanged throughout the four-target fuzz campaign.
Later documentation/CI-only commits do not alter that application snapshot.

Toolchains: Rust 1.99.1; nightly 1.101.0-nightly (2026-10-24), cargo-fuzz 0.13.2,
cargo-llvm-cov 0.9.1, cargo-deny 0.20.2. Linux x86_64. Build caches and TMPDIR used
home storage; Unix-socket fixtures used the short private `/home/blackmyth/.cache/lprs-tmp`.

## Production coverage

Both minimal and optional-BLE workspace tests, plus 18 actual CLI process checks
against each feature executable, passed. The production-only gates passed:

| Scope | Covered / measured lines | Coverage | Required |
| --- | ---: | ---: | ---: |
| Core | 1875 / 1931 | 97.10% | 90% |
| CLI, Linux adapters and daemon | 5610 / 6295 | 89.12% | 80% |
| Changed critical production against `399e775` | 6727 / 7425 | 90.60% | 90% |

LLVM LCOV per-line counters and parsed Rust test-only spans define the metric.
Exclusions contain 1249 core and 3737 service test-only lines. No production
function or failure path was excluded. The critical audit scopes/rationales and
all uncovered changed lines are preserved in its JSON output. Individual files
can remain below 90%; aggregate coverage is not proof of every error transition.

Notable remaining limits include hardware-only direct CLI success/interactive
scan paths, uncommon kernel/descriptor allocation errors, distinct-UID peer
credentials, and some filesystem sync/worker-failure paths. The passing semantic
suite separately checks uncertain outcomes/no replay, address changes, stale
results, bounded cancellation, FIFO rejection, foreign-file ownership, split key
acquisition/redaction, partial batteries and recovery. Native Bluetooth driver,
physical media continuity and real suspend/case behavior still need their
release acceptance records.

Local temporary evidence (not portable repository artifacts):
`/tmp/librepods-rs-bc78c01-production-coverage.{json,lcov}`,
`/tmp/librepods-rs-bc78c01-production-coverage-test-spans.json`,
`/tmp/librepods-rs-bc78c01-production-coverage.log`, and
`/tmp/librepods-rs-bc78c01-changed-coverage.json`.
CI preserves the same artifact types and enforces all three thresholds.

## Bounded fuzz campaign

The standalone locked workspace exercised production AACP, BLE parsing/crypto,
IPC and config APIs with synthetic seeds and
`-max_total_time=30 -max_len=16385 -rss_limit_mb=2048` per target. All processes
exited zero without a crash reproducer:

| Target | Executions | Reported fuzz elapsed |
| --- | ---: | ---: |
| AACP | 12,591,672 | 31 seconds |
| BLE | 10,611,165 | 31 seconds |
| IPC | 2,708,932 | 31 seconds |
| Config | 3,635,444 | 31 seconds |

Compilation time is separate from reported fuzz elapsed. The result record
contains exact HEAD/tree, clean-state checks before/after, actual compiler,
commands, exit codes and counts:
`/tmp/librepods-rs-bc78c01-fuzz-results.json`; raw logs share its prefix.
This bounded campaign is not exhaustive proof. Scheduled CI uses a longer
120-second run per target and preserves failure artifacts.

## Interpreter, dependency and packaging checks

On production checkpoint `6405fc3`, Miri passed 67 pure-core unit tests, 8 protocol
regressions and 1 doctest; the regression suite took 95.45 seconds. Evidence:
`/tmp/librepods-rs-6405fc3-miri.log`. Later orientation-only source changes were
covered by the ordinary stable suites; this Miri result is explicitly historical.

`cargo deny --locked check` passed advisories, bans, licenses and sources at the
recorded earlier integration checkpoint. No advisory exemption was added;
workspace Tokio 1.54.1 resolves the initially detected 1.50 advisory. Recheck the
final lock before release.

Packaging tests operate only on temporary prefixes. Real `systemd-analyze --user
verify` accepted both default and opt-in compatibility unit arguments, including
space/percent/dollar paths. A transient user unit executing a staged `/usr/bin/true`
also accepted the special path with environment expansion disabled; this does
not establish the real daemon sandbox gate. No automatic packaging test starts
or switches the user's real AirPods services.
