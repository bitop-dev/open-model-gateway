-- #840 (guardrails phase 1, docs/guardrails-design.md §9): one row per
-- guardrail decision, partitioned by month like the rest of history. A
-- blocked input has no execution row (it reserves nothing and never reaches a
-- provider), so this table is how Logs shows it.
--
-- Rows hold identifiers, fixed codes and a count, never text: no matched
-- text, spans, offsets, context, scores or detector input. `model` is the
-- catalog model the request named (it matched a deployment before inspection
-- ran), and `operation` the client operation.
--
-- Rules and rule sets are replaced and overrides removed, so the policy
-- columns carry no foreign keys: an event names the rule set, rule and
-- revision that decided, as they were.
SET lock_timeout = '10s';

-- Guardrail history is archived on its own: it starts with this migration,
-- so it has no partitions for the months the other history groups already
-- hold.
ALTER TABLE history_partitions DROP CONSTRAINT history_partitions_archive_group_check;
ALTER TABLE history_partitions ADD CONSTRAINT history_partitions_archive_group_check
 CHECK (archive_group IN ('history','audit','storage','guardrails'));

CREATE TABLE guardrail_events (
 id uuid NOT NULL,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 -- The client request (a gateway-run batch line's own request id).
 root_request_id uuid NOT NULL,
 workspace_id uuid NOT NULL,
 api_key_id uuid NOT NULL,
 -- The client operation: a generation protocol or a workload.
 operation text NOT NULL CHECK (operation IN ('chat_completions','responses','messages','count_tokens','embeddings',
  'rerank','systemone','audio_speech','images','audio_transcriptions')),
 model text NOT NULL CHECK (char_length(model) BETWEEN 1 AND 200),
 -- Whether the client asked for a stream (generation only).
 streamed boolean NOT NULL DEFAULT false,
 stage text NOT NULL CHECK (stage IN ('input','output')),
 rule_set_id uuid NOT NULL,
 rule_id uuid NOT NULL,
 revision bigint NOT NULL CHECK (revision >= 1),
 layer text NOT NULL CHECK (layer IN ('type_default','workspace_override','workspace_local')),
 detector_kind text NOT NULL CHECK (detector_kind IN ('pattern','systemone','llm_judge','external')),
 category text NOT NULL CHECK (category IN ('secret','personal_data','moderation','prompt_injection','custom')),
 action text NOT NULL CHECK (action IN ('monitored','redacted','blocked','stopped','detector_failed','skipped')),
 -- Findings of this rule and category in the request (at least one).
 count integer NOT NULL CHECK (count BETWEEN 1 AND 1000000),
 -- This decision is the one the client was refused with (one per refused
 -- request; such a request has no execution row).
 refused boolean NOT NULL DEFAULT false,
 -- The rule is a platform rule a local tightening made stricter: its action
 -- may be the workspace's own choice (left out of a personal workspace's
 -- platform totals).
 tightened boolean NOT NULL DEFAULT false,
 -- Further decisions of the request not written as rows (on its first row:
 -- at most 16 rows are written per request).
 omitted integer NOT NULL DEFAULT 0 CHECK (omitted BETWEEN 0 AND 10000),
 PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);
CREATE INDEX guardrail_events_request ON guardrail_events (root_request_id, created_at);
CREATE INDEX guardrail_events_workspace_time ON guardrail_events (workspace_id, created_at, id);
CREATE INDEX guardrail_events_refused ON guardrail_events (workspace_id, created_at) WHERE refused;
-- The key is a key of the event's workspace (so the workspace exists too),
-- checked at insert with a plain read, as 0034 does for executions: a
-- foreign key would take FOR KEY SHARE on the workspace and key rows for
-- every event (MultiXacts on authority rows in the hot path). Those rows are
-- never removed or re-keyed (0034's guards), so the read cannot race a
-- removal. A violation raises 23503, like the foreign key would.
CREATE FUNCTION guardrail_events_parent_check() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM public.api_keys k WHERE k.workspace_id=NEW.workspace_id AND k.id=NEW.api_key_id) THEN
  RAISE EXCEPTION 'guardrail event key is not a key of its workspace' USING ERRCODE='23503',
   DETAIL=format('workspace_id=%s api_key_id=%s', NEW.workspace_id, NEW.api_key_id);
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION guardrail_events_parent_check() FROM PUBLIC;
CREATE TRIGGER guardrail_events_parent_check BEFORE INSERT ON guardrail_events
 FOR EACH ROW EXECUTE FUNCTION guardrail_events_parent_check();
CREATE TRIGGER guardrail_events_immutable BEFORE UPDATE OR DELETE ON guardrail_events
 FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER guardrail_events_no_truncate BEFORE TRUNCATE ON guardrail_events
 FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Registered like the converted history tables; no `_p_legacy` partition
-- exists (nothing predates the table), so `legacy_upper` is this month.
INSERT INTO history_partitions(parent,position,partition_key,archive_group,legacy_upper,converted_at)
VALUES ('guardrail_events',6,'created_at','guardrails',omg_month_start(clock_timestamp()),clock_timestamp());
SELECT count(*) FROM omg_ensure_partitions(3,clock_timestamp());
RESET lock_timeout;
