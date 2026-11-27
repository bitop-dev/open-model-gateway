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
-- 3. A video awaiting its stored output says so (`output_pending`, set by the
--    poller when the provider reports the video completed and cleared when
--    the output is stored, given up or the job deleted). Videos completed
--    before this migration await nothing (their output was never stored by
--    the gateway). The claim index holds exactly the awaiting jobs, and the
--    trigger (renamed for its wider job) also refuses an output state at
--    insert: only an existing job's output is awaited, stored or given up.
--
-- Locking: one transaction, holding ACCESS EXCLUSIVE on `deployments` and
-- then `async_jobs` (0097's order) from the first statement to commit.
-- `deployments` is scanned once (its re-added CHECK), `async_jobs` three
-- times (the stray-row count, its re-added CHECKs in one pass, the index
-- build). The migrate Job runs before an upgrade, so replicas of the
-- previous release keep serving meanwhile: their reads and writes of routes
-- and jobs (routing, admission, catalog and management reads, video and
-- batch reads, the job poller, the batch runner and its line admissions)
-- wait for the locks, and those whose own deadlines pass first fail as they
-- would behind any slow statement (an admission with 503, a poll retried at
-- the next one). After commit they continue unchanged: they never read or
-- write `output_pending` (its default is false), never write
-- `output_failure`, and every job they write has both a length and a size
-- (a video) or neither (a batch). `lock_timeout` is 10 s; on a timeout it
-- rolls back and can be rerun.
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
-- CHECK does.
ALTER TABLE deployments
 DROP CONSTRAINT deployments_video_options,
 ADD CONSTRAINT deployments_video_options CHECK (
  (video_sizes IS NULL OR video_seconds IS NOT NULL)
  AND (video_seconds IS NULL OR public.video_seconds_valid(video_seconds))
  AND (video_sizes IS NULL OR public.video_sizes_valid(video_sizes)));

DO $$ DECLARE stray bigint; BEGIN
 SELECT count(*) INTO stray FROM public.async_jobs WHERE kind <> 'video' AND (video_seconds IS NOT NULL OR video_size IS NOT NULL);
 IF stray > 0 THEN
  RAISE EXCEPTION '0098: % non-video job(s) carry a video length or size, which no job may; nothing was changed', stray
   USING HINT = 'These rows were not written by the gateway. Find them with: SELECT id FROM async_jobs WHERE kind <> ''video'' AND (video_seconds IS NOT NULL OR video_size IS NOT NULL)';
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
 IF TG_OP = 'INSERT' AND (NEW.output_pending OR NEW.output_failure IS NOT NULL) THEN
  RAISE EXCEPTION 'a video output state is recorded on an existing job only' USING ERRCODE = '23514';
 END IF;
 IF TG_OP = 'UPDATE' AND OLD.output_failure IS NOT NULL AND NEW.output_failure IS DISTINCT FROM OLD.output_failure THEN
  RAISE EXCEPTION 'a video output failure is recorded once' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION async_job_output_state() FROM PUBLIC;
CREATE TRIGGER async_jobs_output_state BEFORE INSERT OR UPDATE OF output_failure ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION async_job_output_state();

-- The poller claims a video awaiting its output. Such jobs are settled, so
-- `async_jobs_due` (unsettled jobs only) does not hold them; this index holds
-- exactly them, so the claim reads no settled history.
CREATE INDEX async_jobs_video_output_due ON async_jobs(next_poll_at) WHERE output_pending;

RESET lock_timeout;
