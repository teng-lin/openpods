# Fixture provenance

These fixtures contain no real device addresses, keys or user names.

| Artifact | Origin | Changes and limits |
| --- | --- | --- |
| `compat/vendor/Model.js` | `omarchy-pods` `fff7fec600a5b9a61cdb40e93eccbcceb4b8f824`, root `Model.js` | Exact source, used to test the real consumer rather than a replacement parser |
| `compat/vendor/tests/model.test.js` | Same commit, `tests/model.test.js` | Replaced the fixture user's name `GM’s` with `Example’s` in input and expectation; logic is unchanged |
| `compat/vendor/LICENSE` | Same commit, root license | MIT copyright notice retained |
| `status/*.json` | Hand-authored contract fixtures, reviewed against `Model.js` and the reference daemon serializer | Synthetic data; these prove consumer expectations, not Rust serializer integration |
| `protocol/*.hex` | Wire layouts and constants already present at Rust baseline `399e7759053d4b0dfe1d37813b0ca9d0bde7af5c` | Synthetic minimal regression frames derived from tested layouts, not new hardware captures |

The native daemon integration suite must additionally produce at least three
legacy JSON snapshots under `target/compat-fixtures`. Run the same compatibility
harness on those snapshots. A missing generated directory is an integration
failure, not a reason to substitute these hand-authored fixtures.

New real captures must record firmware/model, collection method, expected parse,
and anonymization method. Keep key material and device/user identity out of git.
