-- Drain finalizers holding a job row lock before taking the file table lock.
-- Reference reads can continue while an in-flight sweep releases file locks.
LOCK TABLE async_jobs IN EXCLUSIVE MODE;

-- The sweeper probes each job reference independently. NULL references
-- dominate unrelated job history and need no index entry.
CREATE INDEX async_jobs_output_file_ref ON async_jobs(output_file_id) WHERE output_file_id IS NOT NULL;
CREATE INDEX async_jobs_error_file_ref ON async_jobs(error_file_id) WHERE error_file_id IS NOT NULL;

-- Result references are write-once and jobs cannot be deleted. Keep linked
-- results out of the orphan scan even while they await ordinary retention.
ALTER TABLE stored_files ADD COLUMN batch_result_linked boolean NOT NULL DEFAULT false;
UPDATE stored_files f SET batch_result_linked=true
FROM (
 SELECT output_file_id AS id FROM async_jobs WHERE output_file_id IS NOT NULL
 UNION
 SELECT error_file_id FROM async_jobs WHERE error_file_id IS NOT NULL
) linked
WHERE f.id=linked.id AND f.deleted_at IS NULL AND f.api_purpose='batch_output';

CREATE FUNCTION mark_batch_result_linked() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 IF TG_OP='UPDATE' THEN
  IF NEW.output_file_id IS NOT DISTINCT FROM OLD.output_file_id
   AND NEW.error_file_id IS NOT DISTINCT FROM OLD.error_file_id THEN
   RETURN NEW;
  END IF;
 END IF;
 UPDATE public.stored_files
 SET batch_result_linked=true
 WHERE id IN (NEW.output_file_id, NEW.error_file_id)
  AND deleted_at IS NULL AND api_purpose='batch_output' AND NOT batch_result_linked;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION mark_batch_result_linked() FROM PUBLIC;
CREATE TRIGGER async_jobs_mark_batch_result AFTER INSERT OR UPDATE OF output_file_id,error_file_id ON async_jobs
 FOR EACH ROW WHEN (NEW.output_file_id IS NOT NULL OR NEW.error_file_id IS NOT NULL)
 EXECUTE FUNCTION mark_batch_result_linked();

CREATE INDEX stored_files_orphan_results ON stored_files(committed_at)
 WHERE deleted_at IS NULL AND api_purpose='batch_output' AND NOT batch_result_linked AND committed_at IS NOT NULL;
CREATE INDEX stored_files_sweep_pending ON stored_files(created_at)
 WHERE deleted_at IS NULL AND committed_at IS NULL;
