//! A finished video's stored output (0098): recorded or given up, once, by
//! the poller that holds the job's claim.
use uuid::Uuid;

use super::{Result, job_settlement};
use crate::{jobs::fence::BatchFence, store::Store};

/// Record a video's stored output (write-once, 0021), for the poller claim
/// `fence`: `false` when another writer recorded one first, its output was
/// given up, the video was deleted meanwhile (a delete clears
/// `output_pending`), the claim ended, or the job was marked
/// `upstream_id_reused` while the output was fetched (what was fetched may be
/// another workspace's video). Only a live video output of the job's own
/// workspace is ever linked.
pub(crate) async fn set_video_output(
    store: &Store,
    fence: &BatchFence,
    id: Uuid,
    file: Uuid,
) -> Result<bool> {
    Ok(sqlx::query("UPDATE async_jobs SET output_file_id=$4,output_pending=false WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND runner_lease_until IS NOT NULL AND output_pending AND error_code IS DISTINCT FROM 'upstream_id_reused' AND EXISTS(SELECT 1 FROM stored_files f WHERE f.id=$4 AND f.workspace_id=async_jobs.workspace_id AND f.purpose='video_output' AND f.deleted_at IS NULL AND coalesce(f.expires_at,'infinity')>clock_timestamp())")
        .bind(id)
        .bind(fence.runner)
        .bind(fence.epoch)
        .bind(file)
        .execute(&store.pool)
        .await
        .map_err(job_settlement)?
        .rows_affected()
        == 1)
}
/// Give up a video's awaited output for good (0098), for the poller claim
/// `fence`; nothing when it no longer awaits one or the claim ended.
pub(crate) async fn set_output_failure(
    store: &Store,
    fence: &BatchFence,
    id: Uuid,
    code: &str,
) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET output_failure=$4,output_pending=false WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND runner_lease_until IS NOT NULL AND output_pending")
        .bind(id)
        .bind(fence.runner)
        .bind(fence.epoch)
        .bind(code)
        .execute(&store.pool)
        .await
        .map_err(job_settlement)?;
    Ok(())
}
