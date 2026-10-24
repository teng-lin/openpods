#!/usr/bin/env python3
"""Attach to a running no-BLE daemon and record real elapsed performance evidence."""

import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import time

RELEASE_SECONDS = 24 * 60 * 60


def process_sample(pid):
    data = Path(f"/proc/{pid}/stat").read_text()
    fields = data[data.rfind(")") + 2:].split()
    status = Path(f"/proc/{pid}/status").read_text().splitlines()
    rss = next(int(line.split()[1]) for line in status if line.startswith("VmRSS:"))
    return {"start_ticks": int(fields[19]), "cpu_ticks": int(fields[11]) + int(fields[12]), "rss_kib": rss}


def p95(values):
    return sorted(values)[max(0, math.ceil(len(values) * 0.95) - 1)] if values else None


def summarize(samples, elapsed, failures):
    latencies = [sample["status_ms"] for sample in samples if sample.get("status_ms") is not None]
    rss = [sample["rss_kib"] for sample in samples]
    ticks = os.sysconf("SC_CLK_TCK")
    cpu = 100 * (samples[-1]["cpu_ticks"] - samples[0]["cpu_ticks"]) / ticks / elapsed if len(samples) >= 2 and elapsed > 0 else None
    return {
        "elapsed_seconds": elapsed, "samples": len(samples), "failures": failures,
        "status_latency_p95_ms": p95(latencies), "cpu_percent_one_core": cpu,
        "rss_max_kib": max(rss) if rss else None,
        "rss_first_kib": rss[0] if rss else None, "rss_last_kib": rss[-1] if rss else None,
        "release_duration_met": elapsed >= RELEASE_SECONDS,
        "release_soak_passed": elapsed >= RELEASE_SECONDS and not failures and cpu is not None and cpu < 1 and max(rss, default=0) <= 30 * 1024 and rss[-1] <= rss[0] and p95(latencies) is not None and p95(latencies) < 100,
        "memory_growth_review_required": bool(rss and rss[-1] > rss[0]),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--duration-seconds", type=float, default=RELEASE_SECONDS)
    parser.add_argument("--interval-seconds", type=float, default=5)
    args = parser.parse_args()
    if args.duration_seconds <= 0 or args.interval_seconds <= 0:
        parser.error("duration and interval must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    os.chmod(args.output, 0o700)
    started = time.monotonic()
    samples = []
    failures = []
    first_start = None
    metadata = {"pid": args.pid, "binary": str(args.binary.resolve()), "requested_seconds": args.duration_seconds,
                "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "claim": "attach-only measurement; does not change device settings or claim hardware-cycle coverage"}
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    log = args.output / "samples.jsonl"
    try:
        with log.open("x") as output:
            while True:
                sample = process_sample(args.pid)
                if first_start is None:
                    first_start = sample["start_ticks"]
                if sample["start_ticks"] != first_start:
                    failures.append("PID was reused; original service is no longer running")
                    break
                request_started = time.monotonic()
                result = subprocess.run([str(args.binary.resolve()), "status", "--json"], capture_output=True,
                                        text=True, timeout=3, check=False)
                sample["elapsed_seconds"] = time.monotonic() - started
                sample["status_ms"] = (time.monotonic() - request_started) * 1000
                if result.returncode != 0:
                    failures.append(f"status failed at sample {len(samples)}: exit {result.returncode}")
                else:
                    state = json.loads(result.stdout)
                    if state.get("ble_enabled") is not False:
                        failures.append("status did not explicitly prove BLE disabled")
                    sample["connected"] = state.get("connected")
                    sample["revision"] = state.get("revision")
                    sample["lifecycle"] = state.get("lifecycle")
                samples.append(sample)
                output.write(json.dumps(sample) + "\n")
                output.flush()
                elapsed = time.monotonic() - started
                if elapsed >= args.duration_seconds or failures:
                    break
                time.sleep(min(args.interval_seconds, args.duration_seconds - elapsed))
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        failures.append(str(error))
    except KeyboardInterrupt:
        failures.append("measurement interrupted before requested duration")
    summary = summarize(samples, time.monotonic() - started, failures)
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
