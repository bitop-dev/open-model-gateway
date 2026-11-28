//! Deleting a finished video (#52): the gateway's copy and record first,
//! then the provider's copy, best effort.
use super::*;

impl Jobs {
    /// Delete a finished video. In-progress jobs cannot be deleted (their
    /// accounting is still open). The gateway's stored output goes first: a
    /// database failure there (or a replica without the file store) is an
    /// error and nothing is deleted; once its row is claimed the copy is
    /// unreadable, and an object store that fails to remove the object
    /// leaves it to the file sweeper, the delete going on. Then the job is marked
    /// deleted, which also stops any output being recorded for it (one
    /// recorded since the job was read is claimed for deletion with the mark
    /// and deleted next), then the provider is asked to delete its copy. Once
    /// the job is marked the delete has happened, and the rest is best
    /// effort: the gateway's delete stands whatever the provider answers.
    ///
    /// Returns whether the provider's copy is gone: `true` when it deleted it
    /// or no longer had it (`404`), or when the create was never sent
    /// (`create_failed`); `false` when it offers no delete (`405`), refuses
    /// or fails it, the route is disabled, a create of unknown outcome left
    /// no id to ask about, or the job is marked `upstream_id_reused` (never
    /// asked about again, `Jobs::target`).
    pub async fn delete_video(&self, principal: &Principal, id: &str) -> JobResult<(JobRow, bool)> {
        let job = self.own_video(principal, id).await?;
        if !crate::visibility::may_modify_job(
            &self.store.pool,
            principal.workspace_id,
            crate::visibility::Viewer::of(principal),
            job.id,
        )
        .await?
        {
            return Err(JobError::Forbidden(
                "video_not_owned",
                "Only the creator or a workspace admin can delete this video",
            ));
        }
        if !job.job_state().is_terminal() {
            return Err(JobError::Conflict(
                "video_in_progress",
                "A video can be deleted once it is completed or failed",
            ));
        }
        if let Some(file) = job.output_file_id {
            self.delete_output(&job, file).await?;
        }
        let recorded = store::mark_deleted(&self.store, job.id)
            .await?
            .ok_or(JobError::NotFound)?;
        // From here the delete has happened: what follows is best effort. An
        // output recorded since the job was read was claimed for deletion
        // with the mark (unreadable at once); the file sweeper finishes it if
        // this cannot.
        if let Some(file) = recorded.filter(|f| Some(*f) != job.output_file_id)
            && self.delete_output(&job, file).await.is_err()
        {
            tracing::warn!(job_id = %job.id, "a video output recorded during its delete is left to the file sweeper");
        }
        let upstream_deleted = self.delete_upstream(&job).await;
        Ok((job, upstream_deleted))
    }

    /// Ask the provider to delete a deleted job's copy, best effort: whether
    /// it is gone (see [`Self::delete_video`]). The job is gone for its owner,
    /// so this cannot be retried: a copy not confirmed gone is logged.
    async fn delete_upstream(&self, job: &JobRow) -> bool {
        let deleted = match job.upstream() {
            None => return job.error_code.as_deref() == Some("create_failed"),
            Some(_) if job.error_code.as_deref() == Some(UPSTREAM_ID_REUSED) => false,
            Some(upstream_id) => match self.target(job).await {
                Ok((target, adapter)) => matches!(
                    call(adapter.delete_video(&target, &upstream_id)).await,
                    Ok(VideoDeletion::Deleted)
                ),
                // Marked since `job` was read, or the route is disabled.
                Err(_) => false,
            },
        };
        if !deleted {
            tracing::warn!(job_id = %job.id, "a deleted video's provider copy is not confirmed gone");
        }
        deleted
    }

    /// Delete a video's stored output (it is unreadable from then on, and
    /// the file sweeper finishes a delete the store could not complete).
    async fn delete_output(&self, job: &JobRow, file: Uuid) -> JobResult<()> {
        let files = self.files.as_ref().ok_or(InferenceError::Storage)?;
        files
            .delete(file, Some(job.workspace_id))
            .await
            .map_err(|_| InferenceError::Storage)?;
        Ok(())
    }
}
