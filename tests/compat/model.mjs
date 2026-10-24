// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const directory = path.dirname(fileURLToPath(import.meta.url));
const source = fs.readFileSync(path.join(directory, "vendor/Model.js"), "utf8");
const model = new Function(source + "; return { parseStatus, availableModes };")();

// Execute the actual upstream checks with only a filesystem/runtime adapter.
globalThis.Deno = {
  readTextFileSync: (name) => fs.readFileSync(name, "utf8"),
  exit: (code) => { throw new Error(`Upstream model checks failed (${code})`); },
};
await import("./vendor/tests/model.test.js");

const flag = process.argv.indexOf("--fixtures-dir");
const fixtureDirectory = flag >= 0
  ? process.argv[flag + 1]
  : path.join(directory, "../fixtures/status");
assert.ok(fixtureDirectory, "--fixtures-dir needs a path");
const files = fs.readdirSync(fixtureDirectory).filter((name) => name.endsWith(".json"));
assert.ok(files.length >= 3, "at least three generated lifecycle fixtures are required");
for (const filename of files) {
  const input = JSON.parse(fs.readFileSync(path.join(fixtureDirectory, filename), "utf8"));
  const status = input.status ?? input;
  const parsed = model.parseStatus(JSON.stringify(status));
  assert.equal(parsed.ok, true, `${filename}: legacy parser rejected status`);
  assert.equal(parsed.schemaVersion, 1, `${filename}: schema version changed`);
  assert.equal(parsed.connected, status.connected, `${filename}: connected changed`);
  assert.equal(parsed.noiseMode, status.noise_mode, `${filename}: noise mode changed`);
  assert.equal(parsed.deviceName, status.device_name ?? "", `${filename}: device name changed`);
  for (const [wire, field] of [["left", "left"], ["right", "right"], ["case", "caseBattery"]]) {
    const reading = status[wire];
    assert.equal(parsed[field].level, reading?.available ? reading.level : -1,
      `${filename}: ${wire} availability/value changed`);
  }
  if (input.expected) {
    for (const [field, expected] of Object.entries(input.expected)) {
      assert.deepEqual(parsed[field], expected, `${filename}: ${field} changed`);
    }
  }
}
console.log(`Compatibility: upstream checks and ${files.length} status fixtures passed`);
