-- 0020_wait_step.sql
-- synapse — migration 0020: the 'wait' timer step (scheduled/delayed steps).
--
-- Adds a fifth run_steps kind, 'wait': a time-based (non-human) gate. When the executor reaches
-- a fresh wait step it ARMS `run_steps.next_attempt_at = now() + config.wait_secs` and defers
-- (the run stays `running`); the background worker (migration 0017's synapse_list_drivable_runs
-- already surfaces a leading pending step whose next_attempt_at has elapsed) completes it once
-- the delay passes. This REUSES the retry/backoff `next_attempt_at` machinery — a wait and a
-- retry are both "a pending step that must not run before a wake time", so no new column or
-- discovery predicate is needed; only the kind CHECK is widened.

-- Allow the new kind (the inline CHECK from 0010, widened to 'tool' in 0012). Re-add as NOT
-- VALID then VALIDATE separately: a plain ADD CONSTRAINT holds an ACCESS EXCLUSIVE lock while it
-- full-table-scans to validate existing rows (a downtime risk on a large run_steps in prod),
-- whereas ADD ... NOT VALID takes only a brief lock and VALIDATE CONSTRAINT scans under a lighter
-- SHARE UPDATE EXCLUSIVE lock that permits concurrent reads/writes. (The new CHECK is a SUPERSET
-- of the old, so every existing row already satisfies it — validation cannot fail.)
ALTER TABLE run_steps DROP CONSTRAINT run_steps_kind_check;
ALTER TABLE run_steps ADD CONSTRAINT run_steps_kind_check
    CHECK (kind IN ('transform', 'human_approval', 'fail', 'tool', 'wait')) NOT VALID;
ALTER TABLE run_steps VALIDATE CONSTRAINT run_steps_kind_check;
