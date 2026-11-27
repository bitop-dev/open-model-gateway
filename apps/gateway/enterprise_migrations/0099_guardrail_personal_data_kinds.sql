-- #841: personal-data kinds. A rule that looks for personal data names the
-- kinds it looks for, explicitly: there is no "all kinds" value, so a release
-- that adds a kind never changes what an existing rule looks for. Rules of the
-- other categories (secrets, custom patterns), and tightening entries, name
-- none; rules written before this migration are such rules and are unchanged.
-- Adding a kind is a new migration that widens these two lists, with an
-- entry in the gateway's kind table (`guardrails::personal::KINDS`, which a
-- test compares with this migration's lists), a `Kind` variant and the
-- dashboard's labels.
SET lock_timeout = '10s';

-- No release wrote a personal-data rule (phase 1 refused the category), so
-- one here was written directly. It names no kinds, which the new CHECK
-- refuses: stop and name the rows instead of choosing kinds for them.
DO $$
DECLARE n bigint;
BEGIN
 SELECT count(*) INTO n FROM guardrail_rules WHERE 'personal_data' = ANY(categories);
 IF n > 0 THEN
  RAISE EXCEPTION '0099: % guardrail rule(s) already look for personal_data without kinds', n
   USING ERRCODE = '23514',
   HINT = 'Find them with SELECT id, rule_set_id FROM guardrail_rules WHERE ''personal_data'' = ANY(categories); remove them (or their category), then run migrate again.';
 END IF;
END $$;

CREATE FUNCTION guardrail_kinds_valid(k text[]) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
 SELECT coalesce(array_ndims(k), 1) = 1
  AND k <@ ARRAY['email','phone','card','iban','us_ssn','ip']::text[]
  AND cardinality(k) = (SELECT count(DISTINCT x) FROM unnest(k) x)
$$;

ALTER TABLE guardrail_rules
 ADD COLUMN kinds text[] NOT NULL DEFAULT '{}' CHECK (guardrail_kinds_valid(kinds)),
 -- A personal-data rule names at least one kind; any other rule, and a
 -- tightening entry, names none.
 ADD CONSTRAINT guardrail_rules_kinds CHECK (
  CASE WHEN tightens_rule_id IS NOT NULL THEN cardinality(kinds) = 0
       ELSE ('personal_data' = ANY(categories)) = (cardinality(kinds) > 0) END);

-- The personal-data kind a decision is about: a label from the fixed set
-- above, never matched text. Set only on personal-data decisions that found
-- something; null on every other row (and on rows written before this
-- migration). Logs may show it and the totals may group by it.
-- The column is added without a default, so no row is rewritten, and its
-- CHECK is NOT VALID: every row written before this migration has no kind
-- and so meets it, and a validation would scan every partition of the
-- decision history under this migration's exclusive lock while decisions
-- wait to be written. The CHECK holds for every row written from now on.
ALTER TABLE guardrail_events ADD COLUMN kind text;
ALTER TABLE guardrail_events ADD CONSTRAINT guardrail_events_kind CHECK (
 kind IS NULL OR (kind IN ('email','phone','card','iban','us_ssn','ip') AND category = 'personal_data')) NOT VALID;
COMMENT ON COLUMN guardrail_events.kind IS 'The personal-data kind of a personal_data decision (a fixed label, never matched text); null otherwise.';
RESET lock_timeout;
