-- Fixture for tests/chain_source_pg.rs and tests/chain_log_sourced.rs.
--
-- ⚠ Committed, and idempotent, because the first version of it was built by hand
-- in a scratch database that was later deleted — which left
-- `chain_source_pg.rs` with no runnable fixture at all, and its failures then
-- looked like a code regression. A proof you cannot re-run is not a proof.
--
-- Two suites share one database with DISJOINT execution ids, because they want
-- incompatible shapes: 1001-1003 need `playbook.started` and a deliberately
-- truncated middle; 2001-2004 need `playbook.initialized` (533 of 595 of measured
-- reality) and whole chains.
--
-- Usage:
--   kubectl --context kind-noetl -n noetl port-forward svc/chain-pg 55432:5432
--   psql -h 127.0.0.1 -p 55432 -U postgres -d noetl -f tests/fixtures/chain_fixture.sql
--   NOETL_TEST_PG_URL=postgres://postgres:chainproof@127.0.0.1:55432/noetl cargo test

BEGIN;

CREATE SCHEMA IF NOT EXISTS noetl;

-- A minimal standalone table when running against a scratch database. Against a
-- real noetl schema this is a no-op and the real (partitioned) table is used.
CREATE TABLE IF NOT EXISTS noetl.event (
  event_id bigint PRIMARY KEY,
  execution_id bigint NOT NULL,
  catalog_id bigint,
  event_type text NOT NULL,
  status text,
  created_at timestamp DEFAULT now(),
  prev_event_id bigint,
  parent_event_id bigint,
  parent_execution_id bigint,
  node_id text, node_name text, node_type text,
  context jsonb, result jsonb, meta jsonb, error text, worker_id text
);
CREATE INDEX IF NOT EXISTS ix_event_exec ON noetl.event(execution_id, event_id);

-- ⚠ Scoped delete, not TRUNCATE: the fixture must be restorable without
-- destroying anything else that happens to share the database.
DELETE FROM noetl.event WHERE execution_id IN (1001,1002,1003,2001,2002,2003,2004,2005,2006);

-- ===== chain_source_pg.rs =====
-- 1001: complete and fully linked; head 7 non-terminal -> Advance from "7".
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id) VALUES
 (1,1001,'playbook.started','STARTED',NULL),
 (2,1001,'command.issued','PENDING',1),
 (3,1001,'step.enter','PENDING',2),
 (4,1001,'command.claimed','PENDING',3),
 (5,1001,'command.started','PENDING',4),
 (6,1001,'step.exit','PENDING',5),
 (7,1001,'step.completed','PENDING',6);

-- 1002: event 14 DELIBERATELY ABSENT; 15 points at it -> BlockedAtGap(14 @ 15).
-- ⭐ This hole is the whole point of that suite — do not "fix" it.
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id) VALUES
 (11,1002,'playbook.started','STARTED',NULL),
 (12,1002,'command.issued','PENDING',11),
 (13,1002,'step.enter','PENDING',12),
 (15,1002,'command.started','PENDING',14),
 (16,1002,'step.exit','PENDING',15),
 (17,1002,'step.completed','PENDING',16);

-- 1003: terminal at 23.
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id) VALUES
 (21,1003,'playbook.started','STARTED',NULL),
 (22,1003,'command.issued','PENDING',21),
 (23,1003,'execution.completed','COMPLETED',22);

-- ===== chain_log_sourced.rs (noetl/ai-meta#357) =====
-- 2001: the POST-RESTART shape — THREE null-prev rows, because the server's
-- in-memory head map was cold twice while this execution ran.
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id) VALUES
 (2001,2001,'playbook.initialized','initialized',NULL),
 (2002,2001,'command.issued','PENDING',2001),
 (2003,2001,'step.enter','PENDING',2002),
 (2004,2001,'command.claimed','PENDING',2003),
 (2005,2001,'step.exit','PENDING',NULL),
 (2006,2001,'command.issued','PENDING',NULL),
 (2007,2001,'step.enter','PENDING',2006);

-- 2002: clean, fully linked, still running.
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id) VALUES
 (2011,2002,'playbook.initialized','initialized',NULL),
 (2012,2002,'command.issued','PENDING',2011),
 (2013,2002,'step.enter','PENDING',2012),
 (2014,2002,'command.claimed','PENDING',2013),
 (2015,2002,'command.started','PENDING',2014),
 (2016,2002,'step.exit','PENDING',2015);

-- 2003: the MID-FLIGHT shape — short, running, no terminal event.
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id) VALUES
 (2021,2003,'playbook.initialized','initialized',NULL),
 (2022,2003,'command.issued','PENDING',2021),
 (2023,2003,'step.enter','PENDING',2022);

-- 2004: BIG-PARENT (parent_execution_id set) + terminal.
INSERT INTO noetl.event (event_id, execution_id, event_type, status, prev_event_id, parent_execution_id) VALUES
 (2031,2004,'playbook.initialized','initialized',NULL,2001),
 (2032,2004,'command.issued','PENDING',2031,2001),
 (2033,2004,'execution.completed','COMPLETED',2032,2001);
-- ⚠ 2033's prev was NULL, making 2004 a TWO-ROOT execution by accident. Harmless
-- while the chain was rebuilt from id order; fatal under link-defined ordering
-- (noetl/ai-meta#362), which refuses a forked execution rather than guessing which
-- root is real. 2004 exists to test terminal + parent_execution_id, not forking —
-- 2001 is the deliberate multi-root case.

COMMIT;
