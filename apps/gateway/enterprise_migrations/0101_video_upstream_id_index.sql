-- #52: one upstream video is one job. Recording a video's provider id
-- (`jobs::store::record_video_upstream`) looks for every other video job
-- holding that id on the same upstream account, under an advisory lock of
-- (account, id), on every video create. The 0016 unique index is on
-- (deployment_id, upstream_id), and the account spans deployments, so that
-- lookup scanned every video job. This partial index serves it by the id.
--
-- No row is changed and no constraint is added: the migration only builds
-- the index. Building it holds a SHARE lock on `async_jobs` (reads go on,
-- writes wait) for one scan of the table. Mixed-schema replicas are not
-- supported: drain the gateway first (scale it to 0; docs/operations.md,
-- "Upgrade and migration procedure"), since a replica of the previous
-- release fails readiness once this commits. `lock_timeout` is 10 s, for
-- this transaction only; on a timeout it rolls back and can be rerun.
SET LOCAL lock_timeout = '10s';

CREATE INDEX async_jobs_video_upstream_id ON async_jobs(upstream_id) WHERE kind = 'video';
