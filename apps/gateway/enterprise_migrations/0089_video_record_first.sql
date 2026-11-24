-- A video job is recorded before its provider create (#662): the row exists
-- with no provider id while the create runs, so a provider that accepts the
-- video can never leave it untracked. Such a row records when its create
-- started (`submit_started_at`, immutable once set by `async_job_guard`); its
-- id is filled in once (the guard allows NULL -> value only). Every row this
-- or an earlier release wrote satisfies the new rule, which only adds a case.
--
-- Locks: dropping and re-adding the CHECK takes ACCESS EXCLUSIVE on
-- async_jobs until this migration commits, and the new CHECK is validated by
-- one scan of the table under it (one row per job). Everything that reads or
-- writes jobs waits meanwhile, including batch-line admissions, whose
-- execution rows reference their job. The lock wait is bounded at 10 s; on a timeout
-- the migration rolls back and can be run again.
SET lock_timeout = '10s';
ALTER TABLE async_jobs DROP CONSTRAINT async_jobs_upstream_present;
ALTER TABLE async_jobs ADD CONSTRAINT async_jobs_upstream_present CHECK(
 upstream_id IS NOT NULL
 OR (kind='batch' AND batch_mode IS NOT NULL)
 OR (kind='video' AND submit_started_at IS NOT NULL));
RESET lock_timeout;
