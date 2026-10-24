# Local automated evidence

These results are development evidence, not completed hardware or release gates.
The implementation continues to change; rerun against the final release commit.

## 2026-10-24

Toolchains: Rust 1.99.1; nightly 1.101.0-nightly (2026-10-24), cargo-fuzz
0.13.2, cargo-llvm-cov 0.9.1, cargo-deny 0.20.2. Linux x86_64.

A bounded fuzz campaign used the standalone locked workspace, production AACP,
BLE parsing/crypto, IPC and config APIs, synthetic seeds and
`-max_total_time=60 -max_len=16385 -rss_limit_mb=2048`. All four processes exited
zero without a crash reproducer:

| Target | Executions | Reported elapsed |
| --- | ---: | ---: |
| AACP | 21,910,885 | 61 seconds |
| BLE | 19,212,069 | 61 seconds |
| IPC | 5,958,015 | 61 seconds |
| Config | 5,937,621 | 61 seconds |

The campaign started from daemon source `4b85234`; source was rebased during the
campaign to `790c512`. Therefore these counts cannot establish a single exact
release-commit gate. Reproduce with the scheduled workflow's pinned nightly and
preserve evidence on the final commit. Local raw logs and exit-code records were
written under `/tmp/librepods-rs-integrated-fuzz-<target>{.log,-result.json}`;
temporary files are not portable repository artifacts. Fuzz duration is bounded
and does not prove absence of defects.

On daemon source `790c512`, `cargo deny --locked check` reported advisories, bans,
licenses and sources all passing. No advisory exemption was added. The earlier
Tokio 1.50 advisory is resolved by the workspace lock update to 1.54.1.

The CLI process suite passed seven tests against the actual executable, with
private temporary HOME/XDG paths and native Unix IPC peers. All thirteen prior
runtime integration tests passed after the four reproduced safety defects were
fixed in `790c512`. A follow-up read-only audio timeout test exposed another
incorrect uncertain-outcome classification and remained failing at that commit.

The baseline coverage in [testing.md](testing.md) is historical. Subsequent
measurements improved coverage materially, but the service's complete-source
80% gate was still failing. Use `scripts/coverage.sh` to produce current counters;
do not treat diagnostic reports made with `--ignore-run-fail` as passing tests.
