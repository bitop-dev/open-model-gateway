-- #840 (guardrails phase 1, docs/guardrails-design.md §5): the guardrail
-- policy model. Rules live in rule sets, one per layer and scope, mirroring the
-- limit layers: a workspace-type default, a platform per-workspace override
-- that replaces it, and a workspace local layer, set by the owners and admins
-- of a shared workspace and by the owner of a personal workspace. The local
-- layer adds rules or tightens a platform rule (a tightening entry names the
-- platform rule it tightens); it never weakens or removes one. `local_only`,
-- set on a platform layer, makes every rule whose detector sends text out
-- inactive. Rules store no matched text: a custom pattern is configuration.
--
-- Nothing here takes part in admission: the gateway reads the effective rules
-- before admission, from a per-replica cache invalidated through the new
-- `guardrails` topic, and admission never reads them. No authority lock, no
-- global row, no history table.
SET lock_timeout = '10s';

-- The `guardrails` topic: every change to the two tables below bumps it once
-- per transaction at commit (0028's mechanism).
ALTER TABLE config_versions DROP CONSTRAINT config_versions_topic_check;
ALTER TABLE config_versions ADD CONSTRAINT config_versions_topic_check
 CHECK (topic IN ('access','catalog','guardrails','keys','policy','settings'));
INSERT INTO config_versions(topic) VALUES('guardrails');

CREATE TABLE guardrail_rule_sets (
 id uuid PRIMARY KEY,
 layer text NOT NULL CHECK (layer IN ('type_default','workspace_override','workspace_local')),
 -- type_default: the workspace kind; the other layers: the workspace.
 kind text CHECK (kind IN ('personal','team','project')),
 workspace_id uuid REFERENCES workspaces(id),
 -- Platform layers only: no detector that sends text out is effective.
 local_only boolean,
 -- One more on every change of the set or its rules; recorded on decisions.
 revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
 updated_by uuid NOT NULL REFERENCES users(id),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 CHECK ((layer = 'type_default') = (kind IS NOT NULL)),
 CHECK ((layer = 'type_default') = (workspace_id IS NULL)),
 CHECK ((layer = 'workspace_local') = (local_only IS NULL))
);
CREATE UNIQUE INDEX guardrail_rule_sets_type ON guardrail_rule_sets (kind) WHERE layer = 'type_default';
CREATE UNIQUE INDEX guardrail_rule_sets_workspace ON guardrail_rule_sets (workspace_id, layer) WHERE workspace_id IS NOT NULL;

-- A rule set's identity never changes and its revision moves forward by one;
-- every write names who made it: a current Platform Admin for the platform
-- layers, a current owner or admin of the workspace (the owner of a personal
-- one) for the local layer. Only an override is removed (restoring the type
-- default); the other sets are emptied instead.
CREATE FUNCTION guardrail_rule_sets_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE r guardrail_rule_sets;
BEGIN
 IF TG_OP = 'DELETE' THEN
  IF OLD.layer <> 'workspace_override' THEN
   RAISE EXCEPTION 'only a guardrail override is removed' USING ERRCODE = '23514';
  END IF;
  RETURN OLD;
 END IF;
 IF TG_OP = 'UPDATE' THEN
  IF NEW.id <> OLD.id OR NEW.layer <> OLD.layer OR NEW.kind IS DISTINCT FROM OLD.kind
     OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id OR NEW.revision <> OLD.revision + 1 THEN
   RAISE EXCEPTION 'a guardrail rule set keeps its scope and moves forward one revision' USING ERRCODE = '23514';
  END IF;
 ELSIF NEW.revision <> 1 THEN
  RAISE EXCEPTION 'a guardrail rule set starts at revision 1' USING ERRCODE = '23514';
 END IF;
 NEW.updated_at := clock_timestamp();
 r := NEW;
 IF r.layer = 'workspace_local' THEN
  IF NOT EXISTS (SELECT FROM public.effective_workspace_memberships m
                 WHERE m.workspace_id = r.workspace_id AND m.user_id = r.updated_by AND m.role IN ('owner','admin')) THEN
   RAISE EXCEPTION 'a local guardrail layer is written by an owner or admin of its workspace' USING ERRCODE = '23514';
  END IF;
 ELSIF NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = r.updated_by AND p.role = 'admin') THEN
  RAISE EXCEPTION 'a platform guardrail layer is written by a Platform Admin' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER guardrail_rule_sets_guard BEFORE INSERT OR UPDATE OR DELETE ON guardrail_rule_sets
 FOR EACH ROW EXECUTE FUNCTION guardrail_rule_sets_guard();
-- Policies are changed row by row, so each change is guarded and notified.
CREATE FUNCTION guardrail_no_truncate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'guardrail policies are not truncated' USING ERRCODE = '23514';
END $$;
CREATE TRIGGER guardrail_rule_sets_no_truncate BEFORE TRUNCATE ON guardrail_rule_sets
 FOR EACH STATEMENT EXECUTE FUNCTION guardrail_no_truncate();

-- Each custom pattern is 1 to 512 bytes.
CREATE FUNCTION guardrail_patterns_valid(p text[]) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
 SELECT cardinality(p) <= 50 AND NOT EXISTS (SELECT FROM unnest(p) x WHERE x IS NULL OR octet_length(x) NOT BETWEEN 1 AND 512)
$$;

-- A rule names everything it does. A tightening entry (local layer only)
-- names the platform rule it tightens and only the fields it tightens; the
-- gateway takes the stricter of the two for each, so a later change of the
-- platform rule never turns the entry into a loosening. The platform rule may
-- later be removed, so `tightens_rule_id` has no foreign key: the entry is then
-- shown inactive, never dropped. Values beyond phase 1 (other detectors,
-- redaction, output modes, other workloads) are refused by the gateway on
-- write, not here, so a later phase adds behaviour without changing this table.
CREATE TABLE guardrail_rules (
 id uuid PRIMARY KEY,
 rule_set_id uuid NOT NULL REFERENCES guardrail_rule_sets(id),
 position smallint NOT NULL CHECK (position BETWEEN 0 AND 99),
 tightens_rule_id uuid,
 name text CHECK (char_length(name) BETWEEN 1 AND 100 AND btrim(name) <> '' AND name !~ '[\x01-\x1f\x7f-\x9f]'),
 protection text CHECK (protection IN ('secrets','personal_data','moderation','output_dlp','prompt_injection','custom')),
 detector_kind text CHECK (detector_kind IN ('pattern','systemone','llm_judge','external')),
 -- A later phase's detector model or service; never set in phase 1.
 detector_ref text CHECK (char_length(detector_ref) BETWEEN 1 AND 200),
 categories text[] CHECK (cardinality(categories) BETWEEN 1 AND 5 AND categories <@ ARRAY['secret','personal_data','moderation','prompt_injection','custom']::text[]),
 input_action text CHECK (input_action IN ('off','monitor','redact','block')),
 redact_scope text CHECK (redact_scope IN ('user_text','latest_user_turn','all')),
 output_mode text CHECK (output_mode IN ('off','monitor','stop','block')),
 threshold numeric(4,3) CHECK (threshold BETWEEN 0 AND 1),
 on_error text CHECK (on_error IN ('fail_closed','fail_open')),
 timeout_ms integer CHECK (timeout_ms BETWEEN 1 AND 30000),
 workloads text[] CHECK (cardinality(workloads) BETWEEN 1 AND 8 AND workloads <@ ARRAY['generation','embeddings','rerank','systemone','audio_speech','images','audio_transcriptions','realtime']::text[]),
 uninspectable text CHECK (uninspectable IN ('deny','allow')),
 patterns text[] NOT NULL DEFAULT '{}' CHECK (guardrail_patterns_valid(patterns)),
 UNIQUE (rule_set_id, position),
 CHECK (tightens_rule_id IS NOT NULL OR (name IS NOT NULL AND protection IS NOT NULL AND detector_kind IS NOT NULL
        AND categories IS NOT NULL AND input_action IS NOT NULL AND redact_scope IS NOT NULL AND output_mode IS NOT NULL
        AND on_error IS NOT NULL AND workloads IS NOT NULL AND uninspectable IS NOT NULL)),
 CHECK (tightens_rule_id IS NULL OR (name IS NULL AND protection IS NULL AND detector_kind IS NULL AND detector_ref IS NULL
        AND categories IS NULL AND redact_scope IS NULL AND timeout_ms IS NULL AND workloads IS NULL
        AND uninspectable IS NULL AND cardinality(patterns) = 0
        AND (input_action IS NOT NULL OR output_mode IS NOT NULL OR on_error IS NOT NULL OR threshold IS NOT NULL)))
);
CREATE INDEX guardrail_rules_set ON guardrail_rules (rule_set_id, position);

-- Rules are replaced, never edited in place (the gateway deletes and inserts
-- a set's rules with its revision bump). A tightening entry belongs to a local
-- layer and, when written, names a rule of that workspace's platform layer:
-- its override, or its kind's type default.
CREATE FUNCTION guardrail_rules_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE ws uuid;
BEGIN
 IF TG_OP = 'UPDATE' THEN
  RAISE EXCEPTION 'guardrail rules are replaced, not edited' USING ERRCODE = '23514';
 END IF;
 IF NEW.tightens_rule_id IS NOT NULL THEN
  SELECT s.workspace_id INTO ws FROM public.guardrail_rule_sets s WHERE s.id = NEW.rule_set_id AND s.layer = 'workspace_local';
  IF ws IS NULL THEN
   RAISE EXCEPTION 'only a local guardrail layer tightens a platform rule' USING ERRCODE = '23514';
  END IF;
  IF NOT EXISTS (SELECT FROM public.guardrail_rules r JOIN public.guardrail_rule_sets s ON s.id = r.rule_set_id
                 JOIN public.workspaces w ON w.id = ws
                 WHERE r.id = NEW.tightens_rule_id AND r.tightens_rule_id IS NULL
                   AND ((s.layer = 'workspace_override' AND s.workspace_id = ws) OR (s.layer = 'type_default' AND s.kind = w.kind))) THEN
   RAISE EXCEPTION 'a tightening entry names a rule of its workspace''s platform layer' USING ERRCODE = '23514';
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER guardrail_rules_guard BEFORE INSERT OR UPDATE ON guardrail_rules
 FOR EACH ROW EXECUTE FUNCTION guardrail_rules_guard();
CREATE TRIGGER guardrail_rules_no_truncate BEFORE TRUNCATE ON guardrail_rules
 FOR EACH STATEMENT EXECUTE FUNCTION guardrail_no_truncate();

CREATE CONSTRAINT TRIGGER omg_config_guardrail_rule_sets AFTER INSERT OR UPDATE OR DELETE ON guardrail_rule_sets
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('guardrails');
CREATE CONSTRAINT TRIGGER omg_config_guardrail_rules AFTER INSERT OR UPDATE OR DELETE ON guardrail_rules
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('guardrails');
