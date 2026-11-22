-- Report workers identify their bundled calendar at lease acquisition. Expire
-- the old term before taking locks on report schedules: publishers fence on
-- the lease first and may finish a transaction before this cutover.
ALTER TABLE work_leases ADD COLUMN report_tzdb text;
UPDATE work_leases SET report_tzdb='2026f',expires_at='-infinity' WHERE name='report_scheduler';

-- A row trigger also covers an acquisition statement queued behind the
-- migration. Completion and release of a term do not extend its expiry.
CREATE FUNCTION report_lease_calendar_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
 IF current_setting('gateway.report_tzdb',true) IS DISTINCT FROM NEW.report_tzdb THEN
  RETURN NULL;
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION report_lease_calendar_guard() FROM PUBLIC;
CREATE TRIGGER work_leases_report_calendar BEFORE UPDATE OF holder,epoch,expires_at ON work_leases
 FOR EACH ROW WHEN (OLD.name='report_scheduler' AND NEW.expires_at<>'-infinity')
 EXECUTE FUNCTION report_lease_calendar_guard();

DO $$ BEGIN
 IF EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN
  REVOKE UPDATE(next_run_at,next_period_end,last_period_end) ON report_schedules FROM gateway_runtime;
 END IF;
END $$;

-- Legacy initialized cursors get an empty stamp without a table rewrite.
-- The transactional index build scans the schedules and blocks their writers.
ALTER TABLE report_schedules ADD COLUMN cursor_tzdb text COLLATE "C" NOT NULL DEFAULT '';
CREATE INDEX report_schedules_stale_calendar ON report_schedules(cursor_tzdb,next_period_end,id)
 WHERE state='active' AND deleted_at IS NULL AND next_run_at IS NOT NULL AND next_period_end IS NOT NULL;

CREATE OR REPLACE FUNCTION report_calendar_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
 IF OLD.last_period_end IS NOT NULL AND (NEW.last_period_end IS NULL OR NEW.last_period_end<OLD.last_period_end) THEN
  RAISE EXCEPTION 'report cursor cannot move backwards' USING ERRCODE='23514';
 END IF;
 IF NEW.configuration_revision<>OLD.configuration_revision THEN
  NEW.next_run_at=NULL; NEW.next_period_end=NULL; NEW.cursor_tzdb='';
 END IF;
 RETURN NEW;
END $$;

-- Management initialization and the fenced scheduler share this cursor writer.
-- Runtime has no direct cursor UPDATE: old management SQL must not initialize
-- a different UTC period using obsolete calendar rules during the cutover.
CREATE FUNCTION omg_report_set_cursor(schedule uuid, revision bigint, bundle text,
 due_at timestamptz, period_end timestamptz, delivered_end timestamptz)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE required_bundle text;
BEGIN
 SELECT report_tzdb INTO STRICT required_bundle FROM public.work_leases
  WHERE name='report_scheduler' FOR SHARE;
 IF required_bundle IS DISTINCT FROM bundle THEN
  RAISE EXCEPTION 'report calendar bundle mismatch' USING ERRCODE='55006';
 END IF;
 UPDATE public.report_schedules s SET next_run_at=due_at,next_period_end=period_end,
  last_period_end=coalesce(delivered_end,s.last_period_end),cursor_tzdb=required_bundle
 WHERE s.id=schedule AND s.configuration_revision=revision AND s.deleted_at IS NULL;
 IF NOT FOUND THEN
  RAISE EXCEPTION 'report configuration changed' USING ERRCODE='55006';
 END IF;
END $$;
REVOKE ALL ON FUNCTION omg_report_set_cursor(uuid,bigint,text,timestamptz,timestamptz,timestamptz) FROM PUBLIC;
