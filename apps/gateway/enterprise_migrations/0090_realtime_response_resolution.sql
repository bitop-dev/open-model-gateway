-- Operator resolution of an unknown realtime response (#804). A Platform
-- Admin settles a response whose usage the session never recorded, from
-- authoritative evidence, valued with the session's pinned price; the session
-- settles once its last unknown response is resolved
-- (`governance::realtime::resolve_response`). The response keeps the
-- operator's evidence reference and when it was resolved. Evidence follows
-- the handler's rule (Rust `char::is_control`, the Unicode Cc category; a
-- locale's [[:cntrl:]] can be wider).
--
-- Locks: ACCESS EXCLUSIVE on realtime_responses until commit, with one
-- validating scan for the new CHECK (one row per realtime response); realtime
-- response writes wait meanwhile. Each lock wait is bounded at 10 s; on a
-- timeout the migration rolls back and can be run again.
SET lock_timeout = '10s';
ALTER TABLE realtime_responses
 ADD COLUMN evidence text CHECK(evidence IS NULL OR (char_length(evidence) BETWEEN 1 AND 200 AND evidence !~ '[\x01-\x1f\x7f-\x9f]')),
 ADD COLUMN resolved_at timestamptz,
 ADD CONSTRAINT realtime_responses_resolution CHECK((evidence IS NULL)=(resolved_at IS NULL) AND (evidence IS NULL OR state='settled'));

-- A response settles once: pending -> settled|unknown, and a resolution moves
-- an unknown response to settled with evidence. Nothing else changes a
-- terminal row: not its window, floor, status, completion time or unbounded
-- mark; no counter it recorded is lowered (missing counters are filled in),
-- nor its uncached input where input and cached were both recorded; its
-- actual is at least its floor. A resolved row never changes again.
CREATE OR REPLACE FUNCTION realtime_response_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.execution_id<>OLD.execution_id OR NEW.sequence<>OLD.sequence OR NEW.created_at<>OLD.created_at
  OR NEW.window_hold_microusd IS DISTINCT FROM OLD.window_hold_microusd THEN
  RAISE EXCEPTION 'realtime responses settle once';
 END IF;
 IF OLD.state='pending' THEN
  IF NEW.evidence IS NOT NULL OR NEW.resolved_at IS NOT NULL THEN
   RAISE EXCEPTION 'realtime responses settle once';
  END IF;
  RETURN NEW;
 END IF;
 -- IS NOT TRUE: a comparison with NULL (a recorded counter cleared) refuses too.
 IF (OLD.state='unknown' AND NEW.state='settled' AND OLD.evidence IS NULL AND NEW.evidence IS NOT NULL
   AND NEW.status IS NOT DISTINCT FROM OLD.status AND NEW.completed_at IS NOT DISTINCT FROM OLD.completed_at
   AND NEW.floor_microusd IS NOT DISTINCT FROM OLD.floor_microusd
   AND (NEW.unbounded_cost OR NOT OLD.unbounded_cost)
   AND NEW.actual_microusd>=coalesce(OLD.floor_microusd,0)
   AND (OLD.input_text_tokens IS NULL OR OLD.cached_text_tokens IS NULL
    OR NEW.input_text_tokens-NEW.cached_text_tokens>=OLD.input_text_tokens-OLD.cached_text_tokens)
   AND (OLD.input_audio_tokens IS NULL OR OLD.cached_audio_tokens IS NULL
    OR NEW.input_audio_tokens-NEW.cached_audio_tokens>=OLD.input_audio_tokens-OLD.cached_audio_tokens)
   AND (OLD.input_text_tokens IS NULL OR NEW.input_text_tokens>=OLD.input_text_tokens)
   AND (OLD.cached_text_tokens IS NULL OR NEW.cached_text_tokens>=OLD.cached_text_tokens)
   AND (OLD.input_audio_tokens IS NULL OR NEW.input_audio_tokens>=OLD.input_audio_tokens)
   AND (OLD.cached_audio_tokens IS NULL OR NEW.cached_audio_tokens>=OLD.cached_audio_tokens)
   AND (OLD.output_text_tokens IS NULL OR NEW.output_text_tokens>=OLD.output_text_tokens)
   AND (OLD.output_audio_tokens IS NULL OR NEW.output_audio_tokens>=OLD.output_audio_tokens)) IS NOT TRUE THEN
  RAISE EXCEPTION 'realtime responses settle once';
 END IF;
 RETURN NEW;
END $$;
RESET lock_timeout;
