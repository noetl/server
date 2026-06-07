#!/usr/bin/env python3
"""Regenerate expected.json from events.json by running Python's
fold_replay_state logic (extracted standalone to avoid the noetl
package's transitive-dep chain) and emitting the projection state
subset the Rust parity test structurally compares.

The fold logic below is a VERBATIM EXTRACT of
`noetl/server/api/replay/service.py::fold_replay_state` and its
helpers (`_event_id`, `_meta`, `_payload_ref`, `_payload_summary`,
`_stage_id`, `_frame_id`, `_loop_id`, `_command_id`,
`_business_object_identity`, `_business_object_status`).  Re-sync
this file whenever the Python implementation changes — that's
the load-bearing parity contract.

This script is the **source of truth** for what Python's fold
produces for the parity-harness fixture.  Re-run it whenever:
- events.json changes
- Python's fold logic changes (re-extract the helpers below)
- the structural-parity contract changes

Usage:
    cd repos/server/tests/parity_harness
    python3 regenerate_expected.py

Output: writes expected.json next to this script.

No noetl-package imports.  Standalone Python 3.10+.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import sys
from collections.abc import Iterable, Mapping
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


# ---------------------------------------------------------------
# Verbatim extract from noetl/server/api/replay/service.py
# (helpers + fold_replay_state).  No noetl-package imports —
# this file stands alone.
# ---------------------------------------------------------------


def _event_id(event: Mapping[str, Any]) -> Optional[int]:
    value = event.get("event_id")
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _meta(event: Mapping[str, Any]) -> dict[str, Any]:
    meta = event.get("meta")
    return meta if isinstance(meta, dict) else {}


def _payload_ref(event: Mapping[str, Any]) -> Any:
    if event.get("payload_ref") is not None:
        return event.get("payload_ref")
    result = event.get("result")
    if isinstance(result, dict):
        return result.get("reference")
    return None


def _payload_summary(reference: Any) -> dict[str, Any]:
    if not isinstance(reference, Mapping):
        return {
            "sha256": None,
            "schema_digest": None,
            "row_count": None,
            "media_type": None,
            "ref": None,
        }
    rows_ref = reference.get("rows_ref")
    rows_ref = rows_ref if isinstance(rows_ref, Mapping) else {}
    rows_meta = rows_ref.get("meta")
    rows_meta = rows_meta if isinstance(rows_meta, Mapping) else {}
    rows_ipc = rows_ref.get("ipc")
    rows_ipc = rows_ipc if isinstance(rows_ipc, Mapping) else {}
    return {
        "sha256": (
            reference.get("sha256")
            or rows_meta.get("sha256")
            or rows_ipc.get("sha256")
            or reference.get("digest")
        ),
        "schema_digest": (
            reference.get("schema_digest")
            or rows_meta.get("schema_digest")
            or rows_ipc.get("schema_digest")
        ),
        "row_count": (
            reference.get("row_count")
            or rows_meta.get("row_count")
            or rows_ipc.get("row_count")
        ),
        "media_type": (
            reference.get("media_type")
            or rows_meta.get("media_type")
            or rows_ipc.get("media_type")
        ),
        "ref": reference.get("ref") or rows_ref.get("ref") or reference.get("uri"),
    }


def _stage_id(event: Mapping[str, Any]) -> Optional[str]:
    column_value = event.get("stage_id")
    if column_value is not None:
        return str(column_value)
    aggregate_type = event.get("aggregate_type")
    aggregate_id = event.get("aggregate_id")
    if aggregate_type == "stage" and aggregate_id:
        return str(aggregate_id).removeprefix("stage/")
    meta = _meta(event)
    value = meta.get("stage_id")
    return str(value) if value is not None else None


def _frame_id(event: Mapping[str, Any]) -> Optional[str]:
    column_value = event.get("frame_id")
    if column_value is not None:
        return str(column_value)
    aggregate_type = event.get("aggregate_type")
    aggregate_id = event.get("aggregate_id")
    if aggregate_type == "frame" and aggregate_id:
        return str(aggregate_id).removeprefix("frame/")
    meta = _meta(event)
    value = meta.get("frame_id")
    return str(value) if value is not None else None


def _loop_id(event: Mapping[str, Any]) -> Optional[str]:
    meta = _meta(event)
    for key in ("loop_id", "loop_event_id", "__loop_epoch_id"):
        value = meta.get(key)
        if value is not None:
            return str(value)
    return None


def _command_id(event: Mapping[str, Any]) -> Optional[str]:
    value = event.get("command_id")
    if value is not None:
        return str(value)
    meta = _meta(event)
    value = meta.get("command_id")
    return str(value) if value is not None else None


def _business_object_identity(event: Mapping[str, Any]) -> tuple[str, str, str] | None:
    meta = _meta(event)
    business = meta.get("business_object")
    business = business if isinstance(business, Mapping) else {}

    object_type = (
        business.get("object_type")
        or business.get("type")
        or meta.get("business_object_type")
        or meta.get("object_type")
    )
    object_id = (
        business.get("object_id")
        or business.get("id")
        or meta.get("business_object_id")
        or meta.get("object_id")
    )

    aggregate_type = str(event.get("aggregate_type") or "")
    aggregate_id = event.get("aggregate_id")
    if aggregate_type == "business_object" and aggregate_id is not None:
        parts = [part for part in str(aggregate_id).split("/") if part]
        if parts[:1] == ["business_object"]:
            parts = parts[1:]
        if len(parts) >= 2:
            object_type = object_type or parts[0]
            object_id = object_id or "/".join(parts[1:])
        else:
            object_type = object_type or "business_object"
            object_id = object_id or str(aggregate_id)

    if object_type is None or object_id is None:
        return None
    object_type = str(object_type)
    object_id = str(object_id)
    return f"{object_type}/{object_id}", object_type, object_id


def _business_object_status(event_type: str, status: Any) -> str | None:
    if status is not None and status != "":
        return str(status)
    lowered = event_type.lower()
    if lowered.endswith(".deleted") or lowered.endswith(".removed"):
        return "DELETED"
    if lowered.endswith(".created") or lowered.endswith(".updated") or lowered.endswith(".upserted"):
        return "ACTIVE"
    return None


def fold_replay_state(
    events: Iterable[Mapping[str, Any]],
    *,
    tenant_id: str = "default",
    organization_id: str = "default",
    execution_id: int = 1,
    projection: str = "all",
) -> dict[str, Any]:
    """Fold canonical events into a deterministic lightweight state snapshot.

    Extracted verbatim from
    `noetl/server/api/replay/service.py::fold_replay_state`
    minus the snapshot-seed / base_state / upcaster path
    (R5 R5 surfaces; not used by the parity-harness fixture).
    """
    ordered_events = sorted(events, key=lambda event: (_event_id(event) or 0))
    state: dict[str, Any] = {
        "tenant_id": tenant_id,
        "organization_id": organization_id,
        "execution_id": execution_id,
        "projection": projection,
        "event_count": 0,
        "last_event_id": None,
        "last_event_type": None,
        "execution": {
            "status": "UNKNOWN",
            "last_node_name": None,
            "payload_refs": [],
        },
        "stages": {},
        "frames": {},
        "commands": {},
        "business_objects": {},
        "loops": {},
    }

    for event in ordered_events:
        event_id = _event_id(event)
        event_type = str(event.get("event_type") or "")
        status = event.get("status")
        meta = _meta(event)

        state["event_count"] += 1
        state["last_event_id"] = event_id
        state["last_event_type"] = event_type

        # Execution-level status transitions + last_node_name.
        if event_type in {"playbook.completed", "playbook_completed",
                          "workflow.completed", "execution.completed"}:
            state["execution"]["status"] = "COMPLETED"
        elif event_type in {"playbook.failed", "playbook_failed",
                            "workflow.failed", "execution.failed"}:
            state["execution"]["status"] = "FAILED"
        elif event_type in {"playbook.cancelled", "playbook_cancelled"}:
            state["execution"]["status"] = "CANCELLED"
        elif event_type in {"step.enter", "step_enter", "step_started"}:
            if state["execution"]["status"] == "UNKNOWN":
                state["execution"]["status"] = "RUNNING"
            node = event.get("node_name")
            if node is not None:
                state["execution"]["last_node_name"] = node
        elif event_type in {"step.exit", "step_completed", "command.completed"}:
            node = event.get("node_name")
            if node is not None:
                state["execution"]["last_node_name"] = node

        # Execution-level payload_refs.
        payload_ref = _payload_ref(event)
        if payload_ref is not None:
            state["execution"]["payload_refs"].append(
                {"event_id": event_id, "reference": payload_ref}
            )

        # Stage projection.
        stage_id = _stage_id(event)
        if stage_id:
            stage = state["stages"].setdefault(
                stage_id,
                {
                    "stage_id": stage_id,
                    "status": "UNKNOWN",
                    "kind": meta.get("kind"),
                    "step_name": meta.get("step_name") or event.get("node_name"),
                    "frame_count": 0,
                    "row_count": 0,
                    "events_emitted": 0,
                    "failed_count": 0,
                },
            )
            stage["last_event_id"] = event_id
            if event_type == "stage.opened":
                stage["status"] = "OPEN"
                stage["opened_event_id"] = event_id
            elif event_type == "stage.closed":
                stage["status"] = "CLOSED" if not status else str(status)
                stage["closed_event_id"] = event_id

        # Frame projection.
        frame_id = _frame_id(event)
        if frame_id:
            frame = state["frames"].setdefault(
                frame_id,
                {
                    "frame_id": frame_id,
                    "stage_id": stage_id,
                    "status": "UNKNOWN",
                    "row_count": 0,
                    "attempts": 0,
                    "events_emitted": 0,
                },
            )
            frame["last_event_id"] = event_id
            if stage_id:
                frame["stage_id"] = stage_id
            cmd_id_now = _command_id(event)
            if cmd_id_now:
                frame["command_id"] = cmd_id_now

            if event_type == "frame.dispatched":
                frame["status"] = "CLAIMED"
                frame["claimed_event_id"] = event_id
                attempt = meta.get("attempt") or 1
                try:
                    frame["attempts"] = max(int(frame.get("attempts") or 0), int(attempt))
                except (TypeError, ValueError):
                    pass
            elif event_type == "frame.started":
                frame["status"] = "RUNNING"
            elif event_type == "frame.committed":
                frame["status"] = str(status) if status else "COMPLETED"
                row_count = meta.get("row_count")
                if row_count is not None:
                    try:
                        frame["row_count"] = int(row_count)
                    except (TypeError, ValueError):
                        pass
                emitted = meta.get("events_emitted")
                if emitted is not None:
                    try:
                        frame["events_emitted"] = int(emitted)
                    except (TypeError, ValueError):
                        pass
                frame["terminal_event_id"] = event_id
                frame["output_ref"] = payload_ref
                frame["output_ref_summary"] = _payload_summary(payload_ref)
            elif event_type == "frame.failed":
                frame["status"] = str(status) if status else "FAILED"
                emitted = meta.get("events_emitted")
                if emitted is not None:
                    try:
                        frame["events_emitted"] = int(emitted)
                    except (TypeError, ValueError):
                        pass
                frame["terminal_event_id"] = event_id
                frame["output_ref"] = payload_ref
                frame["output_ref_summary"] = _payload_summary(payload_ref)
            elif event_type == "frame.abandoned":
                frame["status"] = str(status) if status else "ABANDONED"
            elif status:
                frame["status"] = str(status)

        # Command projection.
        command_id = _command_id(event)
        if command_id:
            command = state["commands"].setdefault(
                command_id,
                {
                    "command_id": command_id,
                    "status": "UNKNOWN",
                },
            )
            command["last_event_id"] = event_id
            if stage_id:
                command["stage_id"] = stage_id
            if frame_id:
                command["frame_id"] = frame_id
            wid = event.get("worker_id")
            if wid:
                command["worker_id"] = str(wid)
            if event_type == "command.issued":
                command["status"] = "PENDING"
                command["issued_event_id"] = event_id
            elif event_type == "command.claimed":
                command["status"] = "CLAIMED"
                command["claimed_event_id"] = event_id
            elif event_type == "command.started":
                command["status"] = "RUNNING"
                command["started_event_id"] = event_id
            elif event_type == "command.completed":
                command["status"] = str(status) if status else "COMPLETED"
                command["terminal_event_id"] = event_id
            elif event_type == "command.failed":
                command["status"] = str(status) if status else "FAILED"
                command["terminal_event_id"] = event_id
            elif event_type == "command.cancelled":
                command["status"] = str(status) if status else "CANCELLED"
                command["terminal_event_id"] = event_id
            elif event_type.startswith("command.") and status:
                command["status"] = str(status)

        # Business object projection.
        business_identity = _business_object_identity(event)
        if business_identity:
            object_key, object_type, object_id = business_identity
            business_meta = meta.get("business_object")
            business_meta = business_meta if isinstance(business_meta, Mapping) else {}
            business_object = state["business_objects"].setdefault(
                object_key,
                {
                    "object_key": object_key,
                    "object_type": object_type,
                    "object_id": object_id,
                    "status": "UNKNOWN",
                    "version": 0,
                    "event_count": 0,
                    "first_event_id": event_id,
                    "last_event_id": None,
                    "deleted_event_id": None,
                    "last_event_type": None,
                    "last_payload_ref": None,
                    "payload_refs": [],
                    "attributes": {},
                },
            )
            business_object["last_event_id"] = event_id
            business_object["last_event_type"] = event_type
            business_object["event_count"] = int(business_object.get("event_count") or 0) + 1
            business_object["version"] = int(
                business_meta.get("version")
                or meta.get("business_object_version")
                or business_object["event_count"]
            )

            object_status = _business_object_status(event_type, status)
            if object_status:
                business_object["status"] = object_status
                if object_status == "DELETED":
                    business_object["deleted_event_id"] = event_id

            state_value = business_meta.get("state")
            if isinstance(state_value, Mapping):
                business_object["attributes"] = dict(state_value)
            patch_value = business_meta.get("patch") or business_meta.get("attributes")
            if isinstance(patch_value, Mapping):
                business_object["attributes"].update(dict(patch_value))

            if payload_ref is not None:
                payload_entry = {
                    "event_id": event_id,
                    "reference": payload_ref,
                    "summary": _payload_summary(payload_ref),
                }
                business_object["payload_refs"].append(payload_entry)
                business_object["last_payload_ref"] = payload_entry

        # Loop projection.
        loop_id = _loop_id(event)
        if loop_id:
            loop = state["loops"].setdefault(
                loop_id,
                {
                    "loop_id": loop_id,
                    "step_name": event.get("node_name"),
                    "total": meta.get("collection_size") or meta.get("total"),
                    "done": 0,
                    "failed": 0,
                    "completed": False,
                    "last_event_id": None,
                },
            )
            loop["last_event_id"] = event_id
            if event_type in {"command.completed", "loop.shard.done"}:
                loop["done"] = int(loop.get("done") or 0) + 1
            elif event_type in {"command.failed", "loop.shard.failed"}:
                loop["failed"] = int(loop.get("failed") or 0) + 1
            elif event_type in {"loop.done", "loop.fanin.completed"}:
                loop["completed"] = True

    return state


# ---------------------------------------------------------------
# Harness driver: load events.json → fold → emit structural
# subset to expected.json.
# ---------------------------------------------------------------


def _load_events(events_path: Path) -> list[Mapping[str, Any]]:
    with events_path.open(encoding="utf-8") as fh:
        return json.load(fh)


def _structural_subset(state: Mapping[str, Any]) -> dict[str, Any]:
    """Extract the subset the Rust parity test structurally compares.

    Excluded:
    - `checksum` / `checksum_algorithm` / `projection_checksums` —
      Python and Rust use different digest inputs (Python flat-row
      vs. Rust typed state), so byte-for-byte hex parity is NOT a
      requirement.  The structural fields below ARE.
    - `replay_snapshot` / `upcaster_registry_digest` — not exercised
      by this fixture; the typed-shape harness for those lives in
      the Rust unit tests.
    - `payload_summary.summary` on execution.payload_refs entries —
      Python doesn't pre-compute the per-event summary at this level
      (it only sets it on business_object payload_refs).  The Rust
      port does pre-compute it for consistency, so the parity test
      ignores that field at the execution level.
    """
    out: dict[str, Any] = {
        "event_count": state["event_count"],
        "last_event_id": state.get("last_event_id"),
        "last_event_type": state.get("last_event_type"),
        "execution": dict(state.get("execution") or {}),
        "stages": dict(state.get("stages") or {}),
        "frames": dict(state.get("frames") or {}),
        "commands": dict(state.get("commands") or {}),
        "business_objects": dict(state.get("business_objects") or {}),
        "loops": dict(state.get("loops") or {}),
    }
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--events",
        type=Path,
        default=Path(__file__).parent / "events.json",
        help="Path to events.json fixture (default: events.json beside this script).",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).parent / "expected.json",
        help="Output path (default: expected.json beside this script).",
    )
    args = parser.parse_args()

    events = _load_events(args.events)
    state = fold_replay_state(events)
    subset = _structural_subset(state)

    with args.out.open("w", encoding="utf-8") as fh:
        json.dump(subset, fh, indent=2, sort_keys=True, default=str)
        fh.write("\n")

    print(f"Wrote {args.out} from {args.events} ({len(events)} events).", file=sys.stderr)


if __name__ == "__main__":
    main()
