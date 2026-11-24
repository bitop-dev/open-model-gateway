-- Reservation state transitions and holds, enforced by the database.
--
-- A reservation moves only pending -> settled, pending -> unknown and
-- unknown -> settled, and a settled reservation's state, hold, amount, usage,
-- pricing and resolution markers never change again. An
-- unbounded-cost marker is never cleared. A hold never decreases (NULL, an
-- unbounded hold, counts as the largest) except:
--  * when the reservation settles (its amount counts from then on);
--  * when settlement ends a pending attempt as unknown and records the known
--    floor of an unbounded hold, keeping the unbounded marker;
--  * for a pending realtime session, whose hold follows its responses as they
--    settle (`settle_response`) and whose final write sets the hold it keeps;
--  * for a pending gateway-run batch envelope, which moves hold into a line
--    (`admit_line`).
-- An attempt that ends unknown otherwise keeps its hold: unknown cost retains
-- holds. Every writer already checks this in code, before it writes; the
-- trigger makes a writer that lacks the check fail here instead of changing a
-- balance.
--
-- One BEFORE row trigger on the partitioned parent: PostgreSQL clones it onto
-- every attached partition (including `_p_legacy`), onto partitions attached
-- later (`omg_ensure_partitions`: CREATE TABLE ... LIKE, then ATTACH) and drops
-- the clone from a partition that is detached (archive). Inserts and deletes
-- are not affected. It runs as the writer, which already reads
-- inference_executions and async_jobs.
--
-- Locks: CREATE TRIGGER takes SHARE ROW EXCLUSIVE on the parent and then on
-- each partition until this migration's transaction commits. That blocks
-- writers of the reservation table (admission, settlement, reconciliation),
-- not readers, and scans nothing: the catalog work is milliseconds. Each lock
-- first waits for the open transactions that wrote that table or partition,
-- and writers queue behind the locks already taken, so every wait is bounded:
-- after 10 s on any one lock the migration fails, rolls back (releasing what
-- it took) and can simply be run again.
SET lock_timeout = '10s';
CREATE FUNCTION reservation_transition_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public, pg_temp AS $$
BEGIN
 -- A settled reservation is history: what it cost and why never changes
 -- again (the budget totals and rate counters re-derive spend and usage from
 -- these columns). Its time and owner columns are guarded elsewhere (0030's
 -- partition-key trigger, the execution foreign key).
 IF OLD.state='settled' THEN
  IF (NEW.state,NEW.held_microusd,NEW.actual_microusd,NEW.price_id,NEW.price_tier,NEW.reserved_tokens,
    NEW.request_count,NEW.input_tokens,NEW.output_tokens,NEW.billing_usage,NEW.cost_components,NEW.meter_usage,
    NEW.output_image_variant,NEW.provider_cost_microusd,NEW.unbounded_cost,NEW.free_price_resolved,NEW.batch_envelope_resolved)
   IS DISTINCT FROM (OLD.state,OLD.held_microusd,OLD.actual_microusd,OLD.price_id,OLD.price_tier,OLD.reserved_tokens,
    OLD.request_count,OLD.input_tokens,OLD.output_tokens,OLD.billing_usage,OLD.cost_components,OLD.meter_usage,
    OLD.output_image_variant,OLD.provider_cost_microusd,OLD.unbounded_cost,OLD.free_price_resolved,OLD.batch_envelope_resolved) THEN
   RAISE EXCEPTION 'a settled reservation cannot change'
    USING ERRCODE='23514', CONSTRAINT='reservation_settled_change';
  END IF;
  RETURN NEW;
 END IF;
 IF NEW.state IS DISTINCT FROM OLD.state
  AND NOT ((OLD.state='pending' AND NEW.state IN ('settled','unknown')) OR (OLD.state='unknown' AND NEW.state='settled')) THEN
  RAISE EXCEPTION 'reservation state cannot change from % to %', OLD.state, NEW.state
   USING ERRCODE='23514', CONSTRAINT='reservation_state_transition';
 END IF;
 IF OLD.unbounded_cost AND NOT NEW.unbounded_cost THEN
  RAISE EXCEPTION 'a reservation''s unbounded cost marker cannot be cleared'
   USING ERRCODE='23514', CONSTRAINT='reservation_hold_change';
 END IF;
 -- Settling: from here the amount counts, not the hold. Otherwise a hold
 -- that stays, grows, or becomes NULL (unbounded) is not a decrease. This
 -- is where settlement returns.
 IF NEW.state='settled' OR NEW.held_microusd IS NULL
  OR (OLD.held_microusd IS NOT NULL AND NEW.held_microusd>=OLD.held_microusd) THEN
  RETURN NEW;
 END IF;
 -- A decrease: a smaller hold, or a value where the hold was NULL.
 -- Settlement may record the known floor of an unbounded hold when the
 -- attempt ends unknown, keeping the unbounded marker (greatest(NULL, floor)).
 IF OLD.held_microusd IS NULL AND OLD.state='pending' AND NEW.state='unknown' AND NEW.unbounded_cost THEN
  RETURN NEW;
 END IF;
 -- A pending realtime session's hold follows its responses as they settle,
 -- and its final write sets the hold it retains. A pending gateway-run batch
 -- envelope moves hold into a line.
 IF OLD.state='pending' AND EXISTS(SELECT FROM public.inference_executions e
   WHERE e.id=NEW.execution_id AND e.started_at=NEW.admitted_at
    AND (e.workload_kind='realtime'
     OR (NEW.state='pending' AND e.workload_kind='batches' AND e.batch_job_id IS NULL
      AND EXISTS(SELECT FROM public.async_jobs j WHERE j.execution_id=e.id AND j.batch_mode='gateway')))) THEN
  RETURN NEW;
 END IF;
 RAISE EXCEPTION 'reservation hold cannot decrease from % to % outside settlement', OLD.held_microusd, NEW.held_microusd
  USING ERRCODE='23514', CONSTRAINT='reservation_hold_change';
END $$;
-- No column list: a settled row is checked whatever an update names. The
-- WHEN clause keeps every other update (a lease extension, a usage field of a
-- pending row) from calling the function at all.
CREATE TRIGGER governance_reservations_transition BEFORE UPDATE ON governance_reservations
 FOR EACH ROW WHEN (OLD.state='settled' OR OLD.state IS DISTINCT FROM NEW.state
  OR OLD.held_microusd IS DISTINCT FROM NEW.held_microusd OR OLD.unbounded_cost IS DISTINCT FROM NEW.unbounded_cost)
 EXECUTE FUNCTION reservation_transition_guard();
RESET lock_timeout;
