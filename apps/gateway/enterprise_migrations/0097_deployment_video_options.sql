-- #52: video generation on any upstream that serves the OpenAI video
-- contract. Upstreams of one model accept different clip lengths and sizes,
-- so a route (deployment) states what it takes, and a Platform Admin sets
-- it: the `seconds` it accepts, the first of them used when a request
-- names none, and the `size`s it accepts (none: a request may not name a
-- size, and none is sent). A route without `video_seconds` serves no video
-- request.
--
-- Locking: like every migration this runs in one transaction, with the
-- gateway drained (migrations never run from `serve`). The ALTERs take
-- ACCESS EXCLUSIVE on `deployments` and `async_jobs` and hold it to commit.
-- New columns are nullable with no default: catalog-only changes, no table
-- rewrite. The deployments CHECK scans `deployments` (one row per route).
-- The replaced `async_jobs` checks are added NOT VALID, as in 0058: every
-- existing row satisfied the stricter 0016 check, so no history scan is
-- needed, and every new or updated row is checked. The one scan of
-- `async_jobs` is the build of `async_jobs_video_output_due` below.
SET lock_timeout = '10s';

-- Each size is `WIDTHxHEIGHT`, each side 1 to 9999 without a leading zero,
-- and no size repeats. Checked per element (a joined string would accept one
-- element holding a separator). The runtime role may update the column, so
-- the database checks it, not only the gateway.
CREATE FUNCTION video_sizes_valid(s text[]) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
 SELECT coalesce(array_ndims(s), 1) = 1 AND cardinality(s) BETWEEN 1 AND 32
  AND NOT EXISTS (SELECT FROM unnest(s) x WHERE x IS NULL OR x !~ '^[1-9][0-9]{0,3}x[1-9][0-9]{0,3}$')
  AND (SELECT count(DISTINCT x) FROM unnest(s) x) = cardinality(s)
$$;
-- Each length is 1 to 3600 seconds, and no length repeats.
CREATE FUNCTION video_seconds_valid(s integer[]) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
 SELECT coalesce(array_ndims(s), 1) = 1 AND cardinality(s) BETWEEN 1 AND 32
  AND NOT EXISTS (SELECT FROM unnest(s) x WHERE x IS NULL OR x NOT BETWEEN 1 AND 3600)
  AND (SELECT count(DISTINCT x) FROM unnest(s) x) = cardinality(s)
$$;

ALTER TABLE deployments
 ADD COLUMN video_seconds integer[],
 ADD COLUMN video_sizes text[],
 ADD CONSTRAINT deployments_video_options CHECK (
  (video_sizes IS NULL OR video_seconds IS NOT NULL)
  AND (video_seconds IS NULL OR public.video_seconds_valid(video_seconds))
  AND (video_sizes IS NULL OR public.video_sizes_valid(video_sizes)));

-- A video job no longer always has a size: one is recorded only when the
-- request named it (a route without sizes takes none). Its length is still
-- always known. The 0016 check (`video_seconds` and `video_size` both set
-- exactly for videos) is replaced by the two that remain true.
DO $$ DECLARE c record; BEGIN
 FOR c IN SELECT conname FROM pg_constraint WHERE conrelid='public.async_jobs'::regclass AND contype='c' AND pg_get_constraintdef(oid) LIKE '%video_size IS NOT NULL%' LOOP
  EXECUTE format('ALTER TABLE public.async_jobs DROP CONSTRAINT %I',c.conname);
 END LOOP;
END $$;

-- A finished video's output is stored by the gateway (`output_file_id`, the
-- 0021 write-once column). When it cannot be stored for good, the reason is
-- recorded instead (`output_failure`, a reason code): the job stays
-- completed and charged, and its content answers with that reason. One or
-- the other, never both; once recorded, a reason never changes or clears.
ALTER TABLE async_jobs
 ADD COLUMN output_failure text,
 ADD CONSTRAINT async_jobs_video_seconds CHECK((kind='video')=(video_seconds IS NOT NULL)) NOT VALID,
 ADD CONSTRAINT async_jobs_video_size CHECK(video_size IS NULL OR kind='video') NOT VALID,
 ADD CONSTRAINT async_jobs_output_failure CHECK(output_failure IS NULL OR (
  kind='video' AND state='completed' AND output_file_id IS NULL AND output_failure ~ '^[a-z_]{1,64}$'));

CREATE FUNCTION async_job_output_failure_final() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog AS $$
BEGIN
 IF OLD.output_failure IS NOT NULL AND NEW.output_failure IS DISTINCT FROM OLD.output_failure THEN
  RAISE EXCEPTION 'a video output failure is recorded once' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION async_job_output_failure_final() FROM PUBLIC;
CREATE TRIGGER async_jobs_output_failure_final BEFORE UPDATE OF output_failure ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION async_job_output_failure_final();

-- The poller claims a completed video whose output is neither stored nor
-- failed. Such jobs are settled, so `async_jobs_due` (unsettled jobs only)
-- does not hold them; this index does, so a claim never scans settled
-- history.
CREATE INDEX async_jobs_video_output_due ON async_jobs(next_poll_at)
 WHERE kind='video' AND state='completed' AND output_file_id IS NULL AND output_failure IS NULL AND deleted_at IS NULL;

RESET lock_timeout;
