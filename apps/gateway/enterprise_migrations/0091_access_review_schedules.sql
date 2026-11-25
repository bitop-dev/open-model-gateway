-- #481: access reviews started on a schedule. A Platform Admin defines a
-- schedule: a campaign template (name, subjects, workspace filter, how many
-- days each campaign stays open) and a fixed cadence (monthly, quarterly or
-- yearly) with a start day, and for quarterly and yearly cycles a start month.
-- The gateway's lifecycle job starts each due run once across replicas: under
-- the lifecycle work lease, each run takes the installation row lock (a second
-- scheduler waits for it, then finds the schedule no longer due), and the
-- guards below keep one run per cursor position. A run either starts an ordinary campaign,
-- recorded as started by the schedule and naming the admin who created the
-- schedule as its creator, or is skipped and recorded: while the schedule's
-- previous campaign is still open, at the 20-open limit, or when the snapshot
-- is empty, too large or changed. A missed run (the gateway was down) runs once
-- late; nothing is backfilled. Schedules are paused, resumed and deleted
-- (soft); none of that touches the campaigns they started.
SET lock_timeout = '10s';

CREATE TABLE access_review_schedules (
 id uuid PRIMARY KEY,
 -- The campaign template. Each run's campaign is named "<name> (<period date>)".
 name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100 AND btrim(name) <> '' AND name !~ '[\x01-\x1f\x7f-\x9f]'),
 subjects text[] NOT NULL CHECK (cardinality(subjects) BETWEEN 1 AND 3 AND subjects <@ ARRAY['workspace_memberships','platform_roles','workspace_keys']::text[] AND array_position(subjects, NULL) IS NULL),
 workspace_ids uuid[] NOT NULL DEFAULT '{}' CHECK (cardinality(workspace_ids) <= 50 AND array_position(workspace_ids, NULL) IS NULL),
 open_days integer NOT NULL CHECK (open_days BETWEEN 1 AND 90),
 cadence text NOT NULL CHECK (cadence IN ('monthly','quarterly','yearly')),
 start_day smallint NOT NULL CHECK (start_day BETWEEN 1 AND 31),
 start_month smallint CHECK (start_month BETWEEN 1 AND 12),
 created_by uuid NOT NULL REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 -- The Platform Admin who last changed the template, calendar or pause.
 updated_by uuid NOT NULL REFERENCES users(id),
 -- The next period start (UTC), computed by the gateway from the cadence.
 next_run_at timestamptz NOT NULL,
 paused_at timestamptz,
 paused_by uuid REFERENCES users(id),
 deleted_at timestamptz,
 deleted_by uuid REFERENCES users(id),
 CHECK ((cadence = 'monthly') = (start_month IS NULL)),
 CHECK ((paused_at IS NULL) = (paused_by IS NULL)),
 CHECK ((deleted_at IS NULL) = (deleted_by IS NULL))
);
-- The due scan of the lifecycle job.
CREATE INDEX access_review_schedules_due ON access_review_schedules (next_run_at, id) WHERE paused_at IS NULL AND deleted_at IS NULL;

-- Created by a current Platform Admin with a filter of shared workspaces;
-- created_by and created_at never change; a deleted schedule never changes
-- again; changing the template or calendar, pausing, resuming and deleting
-- name a current Platform Admin (updated_by, paused_by, deleted_by). The
-- cursor never moves to or before a period that already ran, so no period
-- runs twice. Never deleted.
CREATE FUNCTION access_review_schedules_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  NEW.created_at := now();
  NEW.updated_at := now();
  NEW.updated_by := NEW.created_by;
  IF NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.created_by AND p.role = 'admin')
     OR NEW.paused_at IS NOT NULL OR NEW.deleted_at IS NOT NULL THEN
   RAISE EXCEPTION 'an access review schedule is created by a Platform Admin' USING ERRCODE = '23514';
  END IF;
 ELSE
  IF (NEW.id, NEW.created_by, NEW.created_at) IS DISTINCT FROM (OLD.id, OLD.created_by, OLD.created_at)
     OR OLD.deleted_at IS NOT NULL
     OR (NEW.paused_by IS DISTINCT FROM OLD.paused_by AND NEW.paused_by IS NOT NULL
         AND NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.paused_by AND p.role = 'admin'))
     OR (NEW.deleted_by IS NOT NULL
         AND NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.deleted_by AND p.role = 'admin'))
     OR (((NEW.name, NEW.subjects, NEW.workspace_ids, NEW.open_days, NEW.cadence, NEW.start_day, NEW.start_month)
           IS DISTINCT FROM (OLD.name, OLD.subjects, OLD.workspace_ids, OLD.open_days, OLD.cadence, OLD.start_day, OLD.start_month)
          OR (OLD.paused_at IS NOT NULL AND NEW.paused_at IS NULL))
         AND NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.updated_by AND p.role = 'admin')) THEN
   RAISE EXCEPTION 'access review schedules keep their creator and change only by a Platform Admin; a deleted schedule never changes' USING ERRCODE = '23514';
  END IF;
  IF NEW.next_run_at IS DISTINCT FROM OLD.next_run_at
     AND EXISTS (SELECT FROM public.access_review_schedule_runs r WHERE r.schedule_id = NEW.id AND r.period_at >= NEW.next_run_at) THEN
   RAISE EXCEPTION 'an access review schedule never runs a period twice' USING ERRCODE = '23514';
  END IF;
 END IF;
 IF cardinality(NEW.subjects) <> (SELECT count(DISTINCT s) FROM unnest(NEW.subjects) s)
    OR cardinality(NEW.workspace_ids) <> (SELECT count(DISTINCT w) FROM unnest(NEW.workspace_ids) w)
    OR (cardinality(NEW.workspace_ids) > 0 AND NOT NEW.subjects && ARRAY['workspace_memberships','workspace_keys']::text[])
    OR (SELECT count(*) FROM public.workspaces w WHERE w.id = ANY (NEW.workspace_ids) AND w.kind IN ('team','project')) <> cardinality(NEW.workspace_ids) THEN
  RAISE EXCEPTION 'an access review schedule covers shared workspaces' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER access_review_schedules_guard BEFORE INSERT OR UPDATE ON access_review_schedules FOR EACH ROW EXECUTE FUNCTION access_review_schedules_guard();
CREATE TRIGGER access_review_schedules_no_delete BEFORE DELETE ON access_review_schedules FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_schedules_no_truncate BEFORE TRUNCATE ON access_review_schedules FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION access_review_schedules_guard() FROM PUBLIC;

-- A campaign a schedule started names it. The runtime role cannot write this
-- column (deploy/staging/runtime-grants.sql): only the function below sets it.
ALTER TABLE access_review_campaigns ADD COLUMN schedule_id uuid REFERENCES access_review_schedules(id);
-- At most one open campaign per schedule: campaigns never pile up.
CREATE UNIQUE INDEX access_review_campaigns_schedule_open ON access_review_campaigns (schedule_id) WHERE schedule_id IS NOT NULL AND closed_at IS NULL;
CREATE INDEX access_review_campaigns_schedule ON access_review_campaigns (schedule_id, created_at) WHERE schedule_id IS NOT NULL;

-- As in 0080, and a campaign started by a schedule: its template, its
-- creator (who need no longer be a Platform Admin: the schedule acts as the
-- system), and only while the schedule is active. The schedule link is
-- immutable like every other field.
CREATE OR REPLACE FUNCTION access_review_campaigns_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  NEW.created_at := now();
  NEW.created_xid := pg_current_xact_id();
  IF (CASE WHEN NEW.schedule_id IS NULL
        THEN NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.created_by AND p.role = 'admin')
        ELSE NOT EXISTS (SELECT FROM public.access_review_schedules s WHERE s.id = NEW.schedule_id
              AND s.created_by = NEW.created_by AND s.subjects = NEW.subjects AND s.workspace_ids = NEW.workspace_ids
              AND s.paused_at IS NULL AND s.deleted_at IS NULL) END)
     OR NEW.closed_at IS NOT NULL OR NEW.closed_by IS NOT NULL
     OR cardinality(NEW.subjects) <> (SELECT count(DISTINCT s) FROM unnest(NEW.subjects) s)
     OR cardinality(NEW.workspace_ids) <> (SELECT count(DISTINCT w) FROM unnest(NEW.workspace_ids) w)
     OR (cardinality(NEW.workspace_ids) > 0 AND NOT NEW.subjects && ARRAY['workspace_memberships','workspace_keys']::text[])
     OR (SELECT count(*) FROM public.workspaces w WHERE w.id = ANY (NEW.workspace_ids) AND w.kind IN ('team','project')) <> cardinality(NEW.workspace_ids) THEN
   RAISE EXCEPTION 'an access review campaign is started by a Platform Admin or its schedule over shared workspaces' USING ERRCODE = '23514';
  END IF;
  RETURN NEW;
 END IF;
 IF (NEW.id, NEW.name, NEW.subjects, NEW.workspace_ids, NEW.created_by, NEW.created_at, NEW.due_at, NEW.created_xid, NEW.schedule_id)
    IS DISTINCT FROM (OLD.id, OLD.name, OLD.subjects, OLD.workspace_ids, OLD.created_by, OLD.created_at, OLD.due_at, OLD.created_xid, OLD.schedule_id)
    OR OLD.closed_at IS NOT NULL OR NEW.closed_at IS NULL
    OR NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.closed_by AND p.role = 'admin') THEN
  RAISE EXCEPTION 'access review campaigns are immutable; closing happens once' USING ERRCODE = '23514';
 END IF;
 NEW.closed_at := clock_timestamp();
 RETURN NEW;
END $$;



-- One row per schedule and period: the run started a campaign or was skipped,
-- with the reason. Append-only.
CREATE TABLE access_review_schedule_runs (
 id uuid PRIMARY KEY,
 schedule_id uuid NOT NULL REFERENCES access_review_schedules(id),
 -- The period start the run is for (for a late run, the latest one).
 period_at timestamptz NOT NULL,
 ran_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 outcome text NOT NULL CHECK (outcome IN ('started','skipped')),
 reason text CHECK (reason IN ('previous_open','open_limit','empty','too_large','changed')),
 campaign_id uuid UNIQUE REFERENCES access_review_campaigns(id),
 UNIQUE (schedule_id, period_at),
 CHECK ((outcome = 'started') = (campaign_id IS NOT NULL) AND (outcome = 'started') = (reason IS NULL))
);
CREATE INDEX access_review_schedule_runs_recent ON access_review_schedule_runs (schedule_id, ran_at DESC);
-- One run per cursor position: a run is for a period from the schedule's
-- current next_run_at up to the server clock, of an active schedule, and none
-- is recorded there yet (the schedule then moves its cursor past it). A
-- started run names a campaign of its own schedule.
CREATE FUNCTION access_review_schedule_runs_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE cursor_at timestamptz;
BEGIN
 NEW.ran_at := clock_timestamp();
 SELECT s.next_run_at INTO cursor_at FROM public.access_review_schedules s
  WHERE s.id = NEW.schedule_id AND s.paused_at IS NULL AND s.deleted_at IS NULL;
 IF cursor_at IS NULL OR NEW.period_at < cursor_at OR NEW.period_at > NEW.ran_at
    OR EXISTS (SELECT FROM public.access_review_schedule_runs r WHERE r.schedule_id = NEW.schedule_id AND r.period_at >= cursor_at) THEN
  RAISE EXCEPTION 'a schedule run is for its due period, once' USING ERRCODE = '23514';
 END IF;
 IF NEW.campaign_id IS NOT NULL AND NOT EXISTS (SELECT FROM public.access_review_campaigns c WHERE c.id = NEW.campaign_id AND c.schedule_id = NEW.schedule_id) THEN
  RAISE EXCEPTION 'a started run names a campaign of its schedule' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER access_review_schedule_runs_guard BEFORE INSERT ON access_review_schedule_runs FOR EACH ROW EXECUTE FUNCTION access_review_schedule_runs_guard();
CREATE TRIGGER access_review_schedule_runs_immutable BEFORE UPDATE OR DELETE ON access_review_schedule_runs FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_schedule_runs_no_truncate BEFORE TRUNCATE ON access_review_schedule_runs FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION access_review_schedule_runs_guard() FROM PUBLIC;

-- Starts the campaign of a due, active schedule for the period starting at
-- p_period (its template; named "<name> (<period date>)"; open for open_days)
-- and records the started run; the runtime snapshots the items in the same
-- transaction. Refused unless the schedule is due on the server clock, the
-- period has begun and the schedule has no open campaign. The runtime role
-- cannot set a campaign's schedule or record a started run any other way.
CREATE FUNCTION omg_access_review_start_scheduled(p_campaign uuid, p_run uuid, p_schedule uuid, p_period timestamptz) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE s public.access_review_schedules;
BEGIN
 SELECT * INTO s FROM public.access_review_schedules
  WHERE id = p_schedule AND paused_at IS NULL AND deleted_at IS NULL AND next_run_at <= clock_timestamp()
  FOR UPDATE;
 IF NOT FOUND OR p_period IS NULL OR p_period > clock_timestamp()
    OR EXISTS (SELECT FROM public.access_review_campaigns c WHERE c.schedule_id = p_schedule AND c.closed_at IS NULL) THEN
  RAISE EXCEPTION 'a scheduled access review starts only when its schedule is due and its previous campaign is closed' USING ERRCODE = '23514';
 END IF;
 INSERT INTO public.access_review_campaigns(id, name, subjects, workspace_ids, created_by, due_at, schedule_id)
 VALUES (p_campaign, s.name || ' (' || to_char(p_period AT TIME ZONE 'UTC', 'YYYY-MM-DD') || ')', s.subjects, s.workspace_ids,
         s.created_by, now() + make_interval(days => s.open_days), s.id);
 INSERT INTO public.access_review_schedule_runs(id, schedule_id, period_at, outcome, campaign_id)
 VALUES (p_run, s.id, p_period, 'started', p_campaign);
END $$;
REVOKE ALL ON FUNCTION omg_access_review_start_scheduled(uuid, uuid, uuid, timestamptz) FROM PUBLIC;

-- The built-in installation incident of a skipped run, one per run for good
-- (its subject is the run), to Platform Admins, with a fixed summary and the
-- run's reason code as its only detail. As in 0058, NOT VALID: every old row
-- satisfies the new constraints, which only add a kind.
ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check CHECK(kind IN (
 'budget_threshold','spend_spike','error_rate','provider_failing','scim_last_admin',
 'batch_failed','batch_stalled','spend_threshold','partitions_missing','history_orphans',
 'admission_ceiling','key_expiring','aged_unknown_hold','access_review_skipped')) NOT VALID;
ALTER TABLE alert_events DROP CONSTRAINT alert_events_builtin_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_builtin_check CHECK(builtin IN (
 'personal_budget','scim_last_admin','partitions_missing','history_orphans','access_review_skipped')) NOT VALID;
ALTER TABLE alert_events DROP CONSTRAINT alert_events_builtin_shape;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_builtin_shape CHECK(builtin IS NULL
 OR (builtin='personal_budget' AND kind='budget_threshold' AND workspace_id IS NOT NULL)
 OR (builtin='scim_last_admin' AND kind='scim_last_admin' AND workspace_id IS NULL AND provider_connection_id IS NULL)
 OR (builtin='partitions_missing' AND kind='partitions_missing' AND workspace_id IS NULL AND provider_connection_id IS NULL)
 OR (builtin='history_orphans' AND kind='history_orphans' AND workspace_id IS NULL AND provider_connection_id IS NULL)
 OR (builtin='access_review_skipped' AND kind='access_review_skipped' AND workspace_id IS NULL AND provider_connection_id IS NULL
     AND subject_key ~ '^access_review_run:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
     AND level = 1 AND severity = 'warning' AND summary = 'A scheduled access review was skipped'
     AND details = jsonb_build_object('reason', details->>'reason')
     AND details->>'reason' IN ('previous_open','open_limit','empty','too_large','changed'))) NOT VALID;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_access_review_kind CHECK(kind<>'access_review_skipped' OR builtin IS NOT DISTINCT FROM 'access_review_skipped') NOT VALID;
CREATE UNIQUE INDEX alert_events_access_review_run ON alert_events (subject_key) WHERE builtin = 'access_review_skipped';

-- As in 0058, with the skipped-run incident: a Platform Admin incident, its
-- webhook summary kind-only (no schedule or campaign names).
CREATE OR REPLACE FUNCTION alert_webhook_summary(text) RETURNS text LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, public, pg_temp AS $$
 SELECT CASE $1
  WHEN 'budget_threshold' THEN 'A shared workspace budget threshold was reached'
  WHEN 'spend_threshold' THEN 'An installation spend threshold was reached'
  WHEN 'spend_spike' THEN 'A spend spike was detected'
  WHEN 'error_rate' THEN 'An error rate threshold was reached'
  WHEN 'provider_failing' THEN 'An upstream connection is failing'
  WHEN 'batch_failed' THEN 'A batch failed'
  WHEN 'batch_stalled' THEN 'A batch stalled'
  WHEN 'scim_last_admin' THEN 'SCIM tried to remove the last Platform Admin'
  WHEN 'partitions_missing' THEN 'Future history partitions are missing'
  WHEN 'history_orphans' THEN 'History parent verification found orphan rows'
  WHEN 'aged_unknown_hold' THEN 'Holds with unknown cost need review'
  WHEN 'access_review_skipped' THEN 'A scheduled access review was skipped'
  ELSE NULL
 END
$$;
CREATE OR REPLACE FUNCTION alert_webhook_payload(p_delivery uuid, p_event uuid, p_transition text) RETURNS text LANGUAGE plpgsql STABLE SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE e public.alert_events; installation_id uuid; safe_summary text;
BEGIN
 IF p_delivery IS NULL THEN RETURN NULL; END IF;
 SELECT id INTO installation_id FROM public.installation WHERE singleton;
 IF installation_id IS NULL THEN RETURN NULL; END IF;
 IF p_event IS NULL THEN
  IF p_transition IS NOT NULL THEN RETURN NULL; END IF;
  RETURN jsonb_build_object('schema','omg.alert.v1','installation_id',installation_id,'delivery_id',p_delivery,'test',true,'event',NULL)::text;
 END IF;
 IF p_transition IS NULL OR p_transition NOT IN ('fired','resolved') THEN RETURN NULL; END IF;
 SELECT * INTO e FROM public.alert_events WHERE id=p_event;
 IF NOT FOUND THEN RETURN NULL; END IF;
 IF e.kind IN ('key_expiring','admission_ceiling')
  OR (e.kind='aged_unknown_hold' AND (e.workspace_id IS NOT NULL OR p_transition<>'fired'))
  OR (p_transition='resolved' AND e.resolution IS DISTINCT FROM 'cleared')
  OR (e.workspace_id IS NOT NULL AND NOT EXISTS(SELECT FROM public.workspaces w WHERE w.id=e.workspace_id AND w.kind IN ('team','project') AND w.disabled_at IS NULL))
  OR NOT (
   (e.rule_id IS NOT NULL AND EXISTS(SELECT FROM public.alert_rules r WHERE r.id=e.rule_id AND r.scope='installation' AND r.enabled AND r.deleted_at IS NULL AND r.notify_platform_admins))
   OR (e.rule_id IS NULL AND e.workspace_id IS NULL AND e.builtin IN ('scim_last_admin','partitions_missing','history_orphans','access_review_skipped'))
   OR (e.kind='aged_unknown_hold' AND e.workspace_id IS NULL AND e.rule_id IS NULL AND e.builtin IS NULL)
  ) THEN RETURN NULL; END IF;
 safe_summary=public.alert_webhook_summary(e.kind);
 IF safe_summary IS NULL THEN RETURN NULL; END IF;
 RETURN jsonb_build_object(
  'schema','omg.alert.v1','installation_id',installation_id,'delivery_id',p_delivery,'test',false,
  'event',jsonb_build_object('id',e.id,'transition',p_transition,'kind',e.kind,'severity',e.severity,
   'occurred_at',to_char((CASE WHEN p_transition='resolved' THEN e.resolved_at ELSE e.fired_at END) AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
   'summary',safe_summary))::text;
END $$;
RESET lock_timeout;
