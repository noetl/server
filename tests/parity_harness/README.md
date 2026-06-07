# Replay Parity Harness — Phase D R5 R7

Cross-server parity test for the Rust `fold_replay_state` port
(noetl/ai-meta#49 → noetl/server#148).  Feeds a synthetic event
log through both folds and asserts structural equality.

## Files

| File | Owner | Purpose |
| :-- | :-- | :-- |
| `events.json` | hand-authored | Synthetic event log exercising all six replay projections (`execution` / `stage` / `frame` / `command` / `business_object` / `loop`) plus payload refs. |
| `expected.json` | generated | Python's structured fold output for `events.json`, computed by `regenerate_expected.py`.  Committed alongside so the parity test is hermetic. |
| `regenerate_expected.py` | hand-authored | Standalone Python script — verbatim extract of `noetl/server/api/replay/service.py::fold_replay_state` + helpers.  Reads `events.json`, folds, emits `expected.json`.  No `noetl`-package imports (avoids the transitive-dep chain). |
| `../parity_harness.rs` | hand-authored | Rust integration test.  Loads both files; folds events through the Rust port; compares structurally. |

## Parity contract

The harness asserts **structural** parity — same projection keys,
same per-key field values (status, counters, summaries,
references).  This is the load-bearing contract: the Rust port
produces the same logical view as the Python source-of-truth.

The harness explicitly does **NOT** assert byte-for-byte hex
parity on `checksum.value` / `projection_checksums[*].value`.
Python and Rust hash different digest inputs:
- Python feeds the `normalize_replayed_*_projection` flat-row
  layer into SHA-256.
- Rust hashes the typed state directly (per R4's design
  decision — see [server#148
  comment](https://github.com/noetl/server/issues/148#issuecomment-4643219314)).

Both approaches deliver determinism + replay validation; they
just produce different hex output.  The typed `Checksum { type,
value }` shape (R4) keeps the wire shape stable across future
algorithm additions.

## Regenerating the snapshot

When the Python fold logic changes (or when you extend
`events.json`):

```bash
cd tests/parity_harness
python3 regenerate_expected.py
```

The script is standalone Python 3.10+; no `noetl`-package
imports.  Re-sync the verbatim-extracted helpers at the top of
the script whenever `noetl/server/api/replay/service.py`
changes — that's the load-bearing parity contract.

## Running the parity test

```bash
cd repos/server
cargo test --test parity_harness
```

Eight tests should pass:

- `parity_top_level_counts_match`
- `parity_execution_status_and_last_node_name`
- `parity_execution_payload_refs`
- `parity_stage_projection`
- `parity_frame_projection`
- `parity_command_projection`
- `parity_business_object_projection`
- `parity_loop_projection`

Each test runs the Rust fold over the same event log and
compares against the matching slice of `expected.json` —
field-by-field, with helpful failure messages identifying the
diverging key + field.

## What's out of scope

- **Live cross-server execution.**  The harness uses a fixture +
  pre-recorded Python output, not a live Python service.  The
  benefit: reproducible, hermetic, fast, no Python runtime
  required at test time.  The cost: the parity guarantee is only
  as strong as the Python snapshot's accuracy + freshness.
- **Snapshot-seed / base_state path.**  R5 R5 surfaces are
  covered by the Rust unit-test layer, not exercised here.
- **The `checksum` field's hex value** — see "Parity contract"
  above.
