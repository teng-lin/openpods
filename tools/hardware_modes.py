#!/usr/bin/env python3
# LibrePods - AirPods liberated from Apple's ecosystem
# Copyright (C) 2025 LibrePods contributors
# SPDX-License-Identifier: AGPL-3.0-only
"""Explicit real-device mode acceptance; restores the original reported mode."""

import argparse
import hashlib
import json
from pathlib import Path
import signal
import subprocess
import time

MODES = {"Off": "off", "NoiseCancellation": "anc", "Transparency": "transparency", "Adaptive": "adaptive"}


def request(binary, *arguments):
    started = time.monotonic()
    result = subprocess.run([str(binary), *arguments, "--json"], capture_output=True,
                            text=True, timeout=8, check=False)
    if result.returncode:
        raise RuntimeError(f"CLI exited {result.returncode}: {result.stderr.strip()}")
    return json.loads(result.stdout), (time.monotonic() - started) * 1000


def ready(binary, timeout=15):
    deadline = time.monotonic() + timeout
    while True:
        state, _ = request(binary, "status")
        metadata = state["device"]["observations"].get("metadata", {})
        noise = state["device"]["observations"].get("noise_control", {})
        if (state["connected"] and state["device"]["listening_mode"] in MODES
                and metadata and not metadata.get("stale", True)
                and noise and not noise.get("stale", True)):
            return state
        if time.monotonic() >= deadline:
            raise RuntimeError("service did not provide fresh identity and listening mode")
        time.sleep(0.1)


def audio_identity(audio):
    # Do not mistake a new application stream for a policy-caused route change.
    return {key: audio.get(key) for key in ["profile", "default_output", "default_input", "card_name"]}


def run(binary, output, cycles, modes, execute):
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    initial = ready(binary)
    audio, _ = request(binary, "audio", "status")
    original = MODES[initial["device"]["listening_mode"]]
    record = {"claim": "real mode transitions only; does not prove audible continuity, case/suspend cycles, or soak",
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "model": initial["device"]["model_number"], "original_mode": original,
              "initial_audio": audio_identity(audio), "execute": execute, "transitions": [],
              "failures": [], "restoration": "not needed"}
    def save():
        (output / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    save()
    if not execute:
        return record
    try:
        for _ in range(cycles):
            before = ready(binary)
            current = MODES[before["device"]["listening_mode"]]
            target = next(mode for mode in modes if mode != current)
            # Rotate requested modes without counting already-set values as transitions.
            modes = modes[modes.index(target) + 1:] + modes[:modes.index(target) + 1]
            outcome, elapsed = request(binary, "--noise-control", target)
            after = ready(binary)
            now_audio, _ = request(binary, "audio", "status")
            confirmed = MODES[after["device"]["listening_mode"]] == target and outcome == "Confirmed"
            unchanged = audio_identity(now_audio) == record["initial_audio"]
            record["transitions"].append({"elapsed_ms": elapsed, "from": current, "to": target,
                "confirmed": confirmed, "outcome": outcome, "audio_selection_preserved": unchanged,
                "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
            save()
            if not confirmed or not unchanged:
                raise RuntimeError("mode confirmation or audio selection check failed")
            time.sleep(0.2)
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        record["failures"].append(str(error) or "interrupted")
    finally:
        try:
            current = ready(binary)
            if MODES[current["device"]["listening_mode"]] != original:
                request(binary, "--noise-control", original)
            restored = ready(binary)
            now_audio, _ = request(binary, "audio", "status")
            if MODES[restored["device"]["listening_mode"]] != original:
                raise RuntimeError("original mode was not restored")
            if audio_identity(now_audio) != record["initial_audio"]:
                raise RuntimeError("audio selection changed; no automatic route overwrite attempted")
            record["restoration"] = "verified"
        except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            record["restoration"] = "failed"
            record["failures"].append(f"restoration: {error}")
        samples = [item["elapsed_ms"] for item in record["transitions"] if item["confirmed"]]
        record["successful_transitions"] = len(samples)
        record["confirmation_p95_ms"] = sorted(samples)[max(0, (len(samples) * 95 + 99) // 100 - 1)] if samples else None
        save()
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--execute", action="store_true", help="actually change modes; otherwise capture a read-only baseline")
    parser.add_argument("--cycles", type=int, default=20)
    parser.add_argument("--modes", nargs="+", choices=sorted(set(MODES.values())), default=["off", "anc", "transparency"])
    args = parser.parse_args()
    if args.cycles < 1 or len(set(args.modes)) < 2:
        parser.error("positive cycles and at least two distinct modes are required")
    def terminate(_signal, _frame):
        raise KeyboardInterrupt("termination requested")
    signal.signal(signal.SIGTERM, terminate)
    record = run(args.binary.resolve(), args.output, args.cycles, list(dict.fromkeys(args.modes)), args.execute)
    print(json.dumps({key: record.get(key) for key in ["execute", "successful_transitions", "confirmation_p95_ms", "restoration", "failures"]}, indent=2))
    return int(bool(record["failures"]))


if __name__ == "__main__":
    raise SystemExit(main())
