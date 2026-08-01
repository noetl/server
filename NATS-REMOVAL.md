# NATS removal — operator notes (server)

The NATS code is gone (noetl/ai-meta#212). Two things an operator should know:

- **`NOETL_COMMAND_BUS` / `NOETL_EVENT_BUS` must be `ehdb`.** Selecting `nats`
  no longer publishes anywhere. It is logged loudly ("command not delivered" /
  "no transport") rather than silently doing nothing, but it will not work.
- **`GET /api/health` reports `"nats":"removed"`** — a constant, kept so an
  existing scraper does not break on a missing key.

Removed env vars: `NOETL_NATS_URL`, `NOETL_REPLICA_COHERENCE=nats_kv`.
Cross-replica coherence is local-only; the seam remains in `src/coherence.rs`.
