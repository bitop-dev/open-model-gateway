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
 workspace_id uuid NOT NULL REFERENCES workspaces(id),
 api_key_id uuid NOT NULL,
 -- The client operation: a generation protocol or a workload.
 operation text NOT NULL CHECK (operation IN ('chat_completions','responses','messages','embeddings',
  'rerank','systemone','audio_speech','images','audio_transcriptions')),
 model text NOT NULL CHECK (char_length(model) BETWEEN 1 AND 200),
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
 PRIMARY KEY (id, created_at),
 FOREIGN KEY (workspace_id, api_key_id) REFERENCES api_keys(workspace_id, id)
) PARTITION BY RANGE (created_at);
CREATE INDEX guardrail_events_request ON guardrail_events (root_request_id, created_at);
CREATE INDEX guardrail_events_workspace_time ON guardrail_events (workspace_id, created_at, id);
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
