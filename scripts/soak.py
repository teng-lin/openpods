#!/usr/bin/env python3
"""Attach to a running no-BLE daemon and record real elapsed performance evidence."""

import argparse
import hashlib
import json
import math
import os
import re
import select
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


def file_digest(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def process_identity(pid, binary):
    before = process_sample(pid)["start_ticks"]
    actual = Path(f"/proc/{pid}/exe")
    actual_stat = actual.stat()
    identity = {"start_ticks": before, "exe_path": os.readlink(actual),
                "exe_device": actual_stat.st_dev, "exe_inode": actual_stat.st_ino,
                "exe_sha256": file_digest(actual), "requested_binary_sha256": file_digest(binary)}
    if identity["exe_sha256"] != identity["requested_binary_sha256"]:
        raise ValueError("requested client binary does not match the running process executable")
    if process_sample(pid)["start_ticks"] != before:
        raise ValueError("process identity changed while hashing executable")
    return identity


def verify_identity(pid, binary, baseline):
    current = process_identity(pid, binary)
    if any(current[key] != baseline[key] for key in
           ["start_ticks", "exe_device", "exe_inode", "exe_sha256", "requested_binary_sha256"]):
        raise ValueError("process or requested binary changed during measurement")


def journal_volume(unit, since, until, entry_limit=10000, byte_limit=1024 * 1024, timeout=5):
    """Bound process time and retained bytes; return counts, never journal content."""
    if not re.fullmatch(r"[A-Za-z0-9_.@-]+\.service", unit):
        raise ValueError("unit must be a simple .service name")
    command = ["journalctl", "--user", "--unit", unit, "--since", f"@{since}",
               "--until", f"@{until}", "--no-pager", "--output=json", "--lines", str(entry_limit + 1)]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    data = bytearray()
    truncated = False
    timed_out = False
    deadline = time.monotonic() + timeout
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                break
            ready, _, _ = select.select([process.stdout], [], [], remaining)
            if not ready:
                timed_out = True
                break
            block = os.read(process.stdout.fileno(), min(4096, byte_limit + 1 - len(data)))
            if not block:
                break
            data.extend(block)
            if len(data) > byte_limit:
                truncated = True
                break
        if process.poll() is None and (truncated or timed_out):
            process.kill()
        code = process.wait(timeout=max(0.1, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=1)
        timed_out = True
        code = process.returncode
    finally:
        process.stdout.close()
    entries = len(data.splitlines())
    truncated = truncated or entries > entry_limit
    return {"unit": unit, "entries_observed": entries, "bytes_observed": len(data),
            "entry_limit": entry_limit, "byte_limit": byte_limit,
            "truncated": truncated, "timed_out": timed_out, "exit_code": code,
            "complete": code == 0 and not truncated and not timed_out}


def source_evidence_valid(commit, tree_hash, tree_state):
    return bool(commit and tree_hash and re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", commit)
                and re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", tree_hash) and tree_state == "clean")


def p95(values):
    return sorted(values)[max(0, math.ceil(len(values) * 0.95) - 1)] if values else None


def summarize(samples, elapsed, failures, integrity=None):
    integrity = integrity or {}
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
        "integrity": integrity,
        "release_soak_passed": integrity.get("release_evidence_complete") is True and elapsed >= RELEASE_SECONDS and not failures and cpu is not None and cpu < 1 and max(rss, default=0) <= 30 * 1024 and rss[-1] <= rss[0] and p95(latencies) is not None and p95(latencies) < 100,
        "memory_growth_review_required": bool(rss and rss[-1] > rss[0]),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--duration-seconds", type=float, default=RELEASE_SECONDS)
    parser.add_argument("--interval-seconds", type=float, default=5)
    parser.add_argument("--source-commit", help="exact source commit used to build the executable")
    parser.add_argument("--source-tree-hash", help="Git tree object used for the build")
    parser.add_argument("--source-tree-state", choices=["clean", "dirty", "unknown"], default="unknown")
    parser.add_argument("--unit", help="optional user service for bounded journal volume evidence")
    args = parser.parse_args()
    if args.duration_seconds <= 0 or args.interval_seconds <= 0:
        parser.error("duration and interval must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    os.chmod(args.output, 0o700)
    started = time.monotonic()
    started_wall = time.time()
    samples = []
    failures = []
    first_start = None
    baseline = None
    daemon_instance = None
    compiled_no_ble = True
    source_valid = source_evidence_valid(args.source_commit, args.source_tree_hash, args.source_tree_state)
    metadata = {"pid": args.pid, "binary": str(args.binary.resolve()), "requested_seconds": args.duration_seconds,
                "source_commit": args.source_commit, "source_tree_hash": args.source_tree_hash,
                "source_tree_state": args.source_tree_state,
                "source_metadata_supplied_not_build_attestation": True,
                "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "claim": "attach-only measurement; does not change device settings or claim hardware-cycle coverage"}
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    log = args.output / "samples.jsonl"
    try:
        baseline = process_identity(args.pid, args.binary.resolve())
        metadata["process_identity"] = baseline
        (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
        with log.open("x") as output:
            while True:
                verify_identity(args.pid, args.binary.resolve(), baseline)
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
                    if not isinstance(state, dict) or not isinstance(state.get("ble"), dict):
                        raise ValueError("status must contain an object with explicit BLE build diagnostics")
                    if state.get("ble_enabled") is not False:
                        failures.append("status did not explicitly prove BLE disabled")
                    if state.get("ble", {}).get("compiled") is not False:
                        compiled_no_ble = False
                        failures.append("status did not explicitly prove BLE support absent from this build")
                    instance = state.get("daemon_instance")
                    if not isinstance(instance, str) or not instance:
                        failures.append("status did not identify the daemon instance")
                    elif daemon_instance is None:
                        daemon_instance = instance
                    elif daemon_instance != instance:
                        failures.append("daemon IPC instance changed during measurement")
                    sample["ble_compiled"] = state.get("ble", {}).get("compiled")
                    sample["daemon_instance"] = instance
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
    measurement_elapsed = time.monotonic() - started
    journal = None
    if args.unit:
        try:
            journal = journal_volume(args.unit, started_wall, time.time())
        except (OSError, ValueError) as error:
            journal = {"complete": False, "error": str(error)}
    integrity = {"binary_identity_verified": baseline is not None,
                 "source_metadata_complete": source_valid,
                 "compiled_no_ble_proven": bool(samples) and compiled_no_ble,
                 "daemon_instance": daemon_instance, "journal_volume": journal,
                 "release_evidence_complete": baseline is not None and source_valid and bool(samples)
                    and compiled_no_ble and bool(daemon_instance) and journal is not None and journal.get("complete") is True}
    summary = summarize(samples, measurement_elapsed, failures, integrity)
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
