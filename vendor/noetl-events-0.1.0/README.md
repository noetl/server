# noetl-events

Shared event-envelope types for the NoETL Rust components.

`ExecutorEvent` is the wire-format event the NoETL CLI's local-mode
runner, the [noetl-worker](https://github.com/noetl/worker), and
(after EE-4 PR 3) the [noetl-server](https://github.com/noetl/server)
all emit and consume.  Pulling this crate into one place removes the
hand-aligned duplicate types that were drifting between producers
and the server's `POST /api/events` handler.

The envelope is intentionally close to the Python
`EventEmitRequest` shape so events emitted by either stack
project against the same `noetl.event` columns.  See the
noetl/noetl wiki page `handle_event_timing` for the field
catalogue and per-field semantics.

## Types

- `ExecutorEvent` — the wire envelope.
- `EventSink` — trait implementations dispatch to (HTTP, NATS,
  stdout, in-memory test buffer).
- `EventEmitter` — thin wrapper that carries an `execution_id` so
  call sites don't thread it through every `emit`.
- `NoopSink` — drops every event; useful in tests.

## History

Carved out of `noetl-executor::events` as EE-4 of
[noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) so
the noetl-server can depend on the canonical envelope shape
directly rather than maintaining a hand-aligned `EventRequest`
type that drifts every time a field is added.  EE-1, EE-2, EE-3
were progressive wire-shape reconciliation rounds; EE-4 is the
structural consolidation.

## License

MIT.  See `LICENSE` at the repository root.
