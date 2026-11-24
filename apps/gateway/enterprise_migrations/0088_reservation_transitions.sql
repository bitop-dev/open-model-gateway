-- Reservation state transitions and holds, enforced by the database.
--
-- A reservation moves only pending -> settled, pending -> unknown and
-- unknown -> settled; a settled reservation never changes state or hold.
-- Its hold never decreases (NULL, an unbounded hold, counts as the largest)
-- except in the write that ends a pending reservation (settlement, lease
-- expiry, a realtime session's final write), when a pending gateway-run batch
-- envelope transfers hold into one of its lines (`admit_line`), or when a
-- response of a pending realtime session settles and the session's hold moves
-- by that response's actual cost minus its window hold (`settle_response`).
-- Every writer already checks this in code, before it writes; the trigger
-- makes a writer that lacks the check fail here instead of corrupting a
-- balance.
--
-- One BEFORE row trigger on the partitioned parent: PostgreSQL clones it onto
-- every attached partition (including `_p_legacy`), onto partitions attached
-- later (`omg_ensure_partitions`: CREATE TABLE ... LIKE, then ATTACH) and drops
-- the clone from a partition that is detached (archive). It fires only for an
-- UPDATE that names state or held_microusd and changes one of them; inserts,
-- deletes and lease extensions are not affected. It runs as the writer, which
-- already reads inference_executions.
--
-- Locks: CREATE TRIGGER takes SHARE ROW EXCLUSIVE on the parent and on every
-- partition until this migration's transaction commits. That blocks writers
-- of the reservation table (admission, settlement, reconciliation), not
-- readers, and scans nothing: the catalog work is milliseconds. It first
-- waits for every open transaction that wrote a reservation, and writers
-- queue behind it while it waits, so the wait is bounded here: after 10 s
-- the migration fails and rolls back, and can simply be run again.
SET lock_timeout = '10s';
CREATE FUNCTION reservation_transition_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF OLD.state='pending' AND NEW.state IN ('settled','unknown') THEN
  -- Ending a pending reservation (settlement, lease expiry, a realtime
  -- session's final write) may set any hold. The common case returns here.
  RETURN NEW;
 END IF;
 IF NEW.state IS DISTINCT FROM OLD.state AND NOT (OLD.state='unknown' AND NEW.state='settled') THEN
  RAISE EXCEPTION 'reservation state cannot change from % to %', OLD.state, NEW.state
   USING ERRCODE='23514', CONSTRAINT='reservation_state_transition';
 END IF;
 IF NEW.held_microusd IS NOT DISTINCT FROM OLD.held_microusd THEN
  RETURN NEW;
 END IF;
 -- A settled reservation's hold is history. Otherwise only a decrease is
 -- refused: a smaller value, or a value where the hold was NULL (unbounded).
 -- A pending gateway-run batch envelope may move hold into a line, and a
 -- pending realtime session's hold follows its responses as they settle.
 IF OLD.state='settled'
  OR (NEW.held_microusd IS NOT NULL AND (OLD.held_microusd IS NULL OR NEW.held_microusd<OLD.held_microusd)
   AND NOT (OLD.state='pending' AND NEW.state='pending' AND OLD.held_microusd IS NOT NULL
    AND EXISTS(SELECT FROM inference_executions e WHERE e.id=NEW.execution_id
     AND e.started_at=NEW.admitted_at AND e.workload_kind IN ('batches','realtime')))) THEN
  RAISE EXCEPTION 'reservation hold cannot change from % to % outside settlement', OLD.held_microusd, NEW.held_microusd
   USING ERRCODE='23514', CONSTRAINT='reservation_hold_change';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER governance_reservations_transition BEFORE UPDATE OF state,held_microusd ON governance_reservations
 FOR EACH ROW WHEN (OLD.state IS DISTINCT FROM NEW.state OR OLD.held_microusd IS DISTINCT FROM NEW.held_microusd)
 EXECUTE FUNCTION reservation_transition_guard();
RESET lock_timeout;
