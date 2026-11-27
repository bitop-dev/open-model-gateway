-- #52: corrects 0097, which is applied and never edited.
--
-- 1. Route options: the validators also require 1-based arrays (the only
--    ones the gateway reads), and every route's options are checked again
--    under them.
-- 2. The two `async_jobs` checks that replaced the 0016 video check (a job is
--    a video exactly when it has a length; only a video has a size) are
--    validated, not left NOT VALID. The 0016 check also allowed a batch with
--    only a length or only a size; the gateway never wrote one, and such a
--    row would fail its next poll, settlement or delete under the new
--    checks. This migration does not rewrite job history to fit: if any such
--    row exists it stops, naming how many, and nothing of it is applied.
-- 3. A video awaiting its stored output says so (`output_pending`). It is
--    set only by the update that records the video completed, cleared only
--    with an outcome (its output stored or given up) or by a delete, which
--    clears it itself (trigger), so code that does not know the column
--    still deletes. Videos completed before this migration await nothing
--    (the gateway never stored their output). The claim index holds exactly
--    the awaiting jobs, and the trigger (renamed for its wider job) also
--    refuses an output state at insert.
--
-- Rows this stops on: a route whose options 0097 took but these validators
-- refuse (written outside the gateway, e.g. a non-1-based array), and a job
-- that 0016 allowed but the checks refuse (a batch with only a length or
-- only a size; the gateway never wrote one). Either stops the migration
-- with the count and the query that finds them, and nothing of it is
-- applied. It does not rewrite route configuration or job history.
--
-- Locking: one transaction, holding ACCESS EXCLUSIVE on `deployments` and
-- then `async_jobs` (0097's order) from the first statement to commit.
-- `deployments` is scanned twice (the count, the re-added CHECK),
-- `async_jobs` three times (the count, its re-added CHECKs in one pass, the
-- index build). Mixed-schema replicas are not supported: drain the gateway
-- first (scale it to 0; docs/operations.md, "Upgrade and migration
-- procedure"), since a replica of the previous release fails readiness
-- once this commits. A replica still running meanwhile waits for the locks
-- and may deadlock against them (one side is aborted; if it is the
-- migration it rolls back and can be rerun); it writes nothing these checks
-- refuse. `lock_timeout` is 10 s; on a timeout it rolls back and can be
-- rerun.
SET lock_timeout = '10s';
LOCK TABLE deployments, async_jobs IN ACCESS EXCLUSIVE MODE;

CREATE OR REPLACE FUNCTION video_sizes_valid(s text[]) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
 SELECT coalesce(array_ndims(s), 1) = 1 AND coalesce(array_lower(s, 1), 1) = 1 AND cardinality(s) BETWEEN 1 AND 32
  AND NOT EXISTS (SELECT FROM unnest(s) x WHERE x IS NULL OR x !~ '^[1-9][0-9]{0,3}x[1-9][0-9]{0,3}$')
  AND (SELECT count(DISTINCT x) FROM unnest(s) x) = cardinality(s)
$$;
CREATE OR REPLACE FUNCTION video_seconds_valid(s integer[]) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
 SELECT coalesce(array_ndims(s), 1) = 1 AND coalesce(array_lower(s, 1), 1) = 1 AND cardinality(s) BETWEEN 1 AND 32
  AND NOT EXISTS (SELECT FROM unnest(s) x WHERE x IS NULL OR x NOT BETWEEN 1 AND 3600)
  AND (SELECT count(DISTINCT x) FROM unnest(s) x) = cardinality(s)
$$;
-- Replacing a function does not check existing rows again: re-adding the
-- CHECK does, after naming any route it would refuse.
DO $$ DECLARE refused bigint; BEGIN
 SELECT count(*) INTO refused FROM public.deployments WHERE (video_seconds IS NOT NULL AND NOT public.video_seconds_valid(video_seconds)) OR (video_sizes IS NOT NULL AND NOT public.video_sizes_valid(video_sizes));
 IF refused > 0 THEN
  RAISE EXCEPTION '0098: % route(s) have video options the gateway cannot read (find them with: SELECT id FROM deployments WHERE (video_seconds IS NOT NULL AND NOT video_seconds_valid(video_seconds)) OR (video_sizes IS NOT NULL AND NOT video_sizes_valid(video_sizes))); set them again with the route PATCH or clear them, then rerun. Nothing of 0098 was applied', refused;
 END IF;
END $$;
ALTER TABLE deployments
 DROP CONSTRAINT deployments_video_options,
 ADD CONSTRAINT deployments_video_options CHECK (
  (video_sizes IS NULL OR video_seconds IS NOT NULL)
  AND (video_seconds IS NULL OR public.video_seconds_valid(video_seconds))
  AND (video_sizes IS NULL OR public.video_sizes_valid(video_sizes)));

DO $$ DECLARE stray bigint; BEGIN
 SELECT count(*) INTO stray FROM public.async_jobs WHERE kind <> 'video' AND (video_seconds IS NOT NULL OR video_size IS NOT NULL);
 IF stray > 0 THEN
  RAISE EXCEPTION '0098: % non-video job(s) carry a video length or size, which no job may (find them with: SELECT id FROM async_jobs WHERE kind <> ''video'' AND (video_seconds IS NOT NULL OR video_size IS NOT NULL)); see the release notes for what to do, then rerun. Nothing of 0098 was applied', stray;
 END IF;
END $$;

DROP TRIGGER async_jobs_output_failure_final ON async_jobs;
DROP FUNCTION async_job_output_failure_final();
DROP INDEX async_jobs_video_output_due;

ALTER TABLE async_jobs
 DROP CONSTRAINT async_jobs_video_seconds,
 DROP CONSTRAINT async_jobs_video_size,
 DROP CONSTRAINT async_jobs_output_failure,
 ADD COLUMN output_pending boolean NOT NULL DEFAULT false,
 ADD CONSTRAINT async_jobs_video_seconds CHECK((kind='video')=(video_seconds IS NOT NULL)),
 ADD CONSTRAINT async_jobs_video_size CHECK(video_size IS NULL OR kind='video'),
 -- Never a file and a reason; nothing awaits an output that is stored,
 -- given up or deleted.
 ADD CONSTRAINT async_jobs_video_output CHECK(
  (output_failure IS NULL OR (kind='video' AND state='completed' AND output_file_id IS NULL AND output_failure ~ '^[a-z_]{1,64}$'))
  AND (NOT output_pending OR (kind='video' AND state='completed' AND output_file_id IS NULL AND output_failure IS NULL AND deleted_at IS NULL)));

CREATE FUNCTION async_job_output_state() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  IF NEW.output_pending OR NEW.output_failure IS NOT NULL THEN
   RAISE EXCEPTION 'a video output state is recorded on an existing job only' USING ERRCODE = '23514';
  END IF;
  RETURN NEW;
 END IF;
 -- A delete ends the wait (nothing is stored for a deleted job).
 IF NEW.deleted_at IS NOT NULL AND OLD.deleted_at IS NULL THEN
  NEW.output_pending := false;
 END IF;
 IF OLD.output_failure IS NOT NULL AND NEW.output_failure IS DISTINCT FROM OLD.output_failure THEN
  RAISE EXCEPTION 'a video output failure is recorded once' USING ERRCODE = '23514';
 END IF;
 IF NEW.output_pending AND NOT OLD.output_pending AND NOT (OLD.state <> 'completed' AND NEW.state = 'completed') THEN
  RAISE EXCEPTION 'a video awaits its output only from the update that records it completed' USING ERRCODE = '23514';
 END IF;
 IF OLD.output_pending AND NOT NEW.output_pending
  AND NEW.output_file_id IS NULL AND NEW.output_failure IS NULL AND NEW.deleted_at IS NULL THEN
  RAISE EXCEPTION 'a video stops awaiting its output only with its output stored or given up, or deleted' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION async_job_output_state() FROM PUBLIC;
CREATE TRIGGER async_jobs_output_state BEFORE INSERT OR UPDATE OF output_pending, output_failure, deleted_at ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION async_job_output_state();

-- The poller claims a video awaiting its output. Once it is settled,
-- `async_jobs_due` (unsettled jobs only) no longer holds it; this index
-- holds exactly the awaiting jobs, so the claim reads no settled history.
CREATE INDEX async_jobs_video_output_due ON async_jobs(next_poll_at) WHERE output_pending;

RESET lock_timeout;
