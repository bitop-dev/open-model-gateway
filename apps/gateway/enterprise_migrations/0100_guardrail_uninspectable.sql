-- #841: content a rule cannot read. A rule says what happens to it
-- (`uninspectable`: `deny` refuses the request, `allow` lets it through
-- recorded as skipped), and `allow` is the weaker value: a local layer's
-- tightening entry may move a platform rule from `allow` to `deny`, never
-- the other way. Rules may now name the workloads this release never reads
-- (images, transcription, realtime), and a realtime session's decisions are
-- recorded like any other request's. Each decision records its rule's own
-- verdict, so content refused is counted apart from content let through.
SET lock_timeout = '10s';

-- A tightening entry names only what it tightens (0092): now also
-- `uninspectable`, and only as `deny`. The 0092 CHECK has a generated name,
-- so it is found by its definition; exactly one must match.
DO $$
DECLARE c record; n int := 0;
BEGIN
 FOR c IN SELECT conname FROM pg_constraint
   WHERE conrelid = 'public.guardrail_rules'::regclass AND contype = 'c'
     AND pg_get_constraintdef(oid) LIKE '%(tightens_rule_id IS NULL) OR ((name IS NULL)%' LOOP
  EXECUTE format('ALTER TABLE public.guardrail_rules DROP CONSTRAINT %I', c.conname);
  n := n + 1;
 END LOOP;
 IF n <> 1 THEN
  RAISE EXCEPTION '0100: expected one tightening-entry CHECK on guardrail_rules, found %', n
   USING ERRCODE = '23514',
   HINT = 'Compare with SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = ''public.guardrail_rules''::regclass AND contype = ''c''; the 0092 CHECK names what a tightening entry may set.';
 END IF;
END $$;

-- Validated in place (a scan of the small rules table under this
-- migration's lock). No stored row can violate it: every row met the 0092
-- CHECK it replaces, which required `uninspectable` to be null on a
-- tightening entry and one of the four other fields to be set, and both
-- imply this one.
ALTER TABLE guardrail_rules ADD CONSTRAINT guardrail_rules_tightening CHECK (
 tightens_rule_id IS NULL OR (name IS NULL AND protection IS NULL AND detector_kind IS NULL AND detector_ref IS NULL
  AND categories IS NULL AND redact_scope IS NULL AND timeout_ms IS NULL AND workloads IS NULL
  AND (uninspectable IS NULL OR uninspectable = 'deny') AND cardinality(patterns) = 0
  AND (input_action IS NOT NULL OR output_mode IS NOT NULL OR on_error IS NOT NULL OR threshold IS NOT NULL
       OR uninspectable IS NOT NULL)));

-- A realtime session's decisions name their operation. The list only
-- grows, so every row written before meets the new CHECK: it is added
-- NOT VALID, on the partitioned table and so on every month partition,
-- existing and created later, without scanning the decision history under
-- this migration's lock. It holds for every row written from now on.
DO $$
DECLARE c record; n int := 0;
BEGIN
 FOR c IN SELECT conname FROM pg_constraint
   WHERE conrelid = 'public.guardrail_events'::regclass AND contype = 'c'
     AND pg_get_constraintdef(oid) LIKE '%operation = ANY%' LOOP
  EXECUTE format('ALTER TABLE public.guardrail_events DROP CONSTRAINT %I', c.conname);
  n := n + 1;
 END LOOP;
 IF n <> 1 THEN
  RAISE EXCEPTION '0100: expected one operation CHECK on guardrail_events, found %', n
   USING ERRCODE = '23514',
   HINT = 'Compare with SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = ''public.guardrail_events''::regclass AND contype = ''c''; the 0095 CHECK lists the operations.';
 END IF;
END $$;
ALTER TABLE guardrail_events ADD CONSTRAINT guardrail_events_operation CHECK (
 operation IN ('chat_completions','responses','messages','count_tokens','embeddings',
  'rerank','systemone','audio_speech','images','audio_transcriptions','realtime')) NOT VALID;

-- Whether the decision's rule refuses the request (its own verdict).
-- `refused` marks the one decision the client was refused with, so it
-- cannot tell a rule that let unreadable content through from one whose
-- refusal another decision outranked; this can. Added without a default, so
-- no row is rewritten; null on every row written before (their rule's
-- verdict was not recorded).
ALTER TABLE guardrail_events ADD COLUMN refuses boolean;
COMMENT ON COLUMN guardrail_events.refuses IS 'Whether the decision''s rule refuses its request (its own verdict, whatever other decisions did); null on rows written before migration 0100.';
RESET lock_timeout;
