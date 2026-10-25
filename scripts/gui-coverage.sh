#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
report="${1:-$PWD/target/gui-coverage.json}"
mkdir -p "$(dirname "$report")"
cargo llvm-cov --manifest-path librepods-gui/Cargo.toml --no-default-features --locked --json --output-path "$report"
python3 - "$report" <<'PY'
import json, sys
from pathlib import Path
report=json.loads(Path(sys.argv[1]).read_text())
files={Path(f['filename']).name:f['summary']['lines']
       for data in report['data'] for f in data['files']
       if '/librepods-gui/src/' in f['filename'].replace('\\','/')}
failed=False
for name, minimum in [('model.rs',95),('client.rs',90),('demo.rs',95)]:
    rows=files.get(name, {'covered':0,'count':0,'percent':0})
    print(f"{name}: {rows['covered']}/{rows['count']} ({rows['percent']:.2f}%)")
    failed |= rows['count']==0 or rows['percent']<minimum
if failed:
    raise SystemExit('GUI production line coverage below its per-file gate')
PY
