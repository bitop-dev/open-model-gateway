//! A finished video's output: the poller downloads it once from its provider
//! into the gateway's file store, and `/content` serves that copy.
//!
//! The job is settled (charged) when the provider reports it completed; the
//! output is stored afterwards. A completed job is charged whether or not
//! its output could be stored: the provider produced it. When the output
//! cannot be stored, the job says so for good (`output_failure`, 0098) and
//! `/content` answers with that reason, never "try again later":
//! - at once, when a retry cannot change the outcome: it is empty or larger
//!   than [`VIDEO_OUTPUT_MAX_BYTES`], the file store or its video outputs are
//!   turned off, or the server gave the video's id to another job;
//! - after [`VIDEO_OUTPUT_ATTEMPTS`] failed attempts otherwise (the poller's
//!   backoff spaces them: the poll interval doubled per failure), with the
//!   last attempt's cause. A refusal from the server (a 4xx) is retried too:
//!   a content endpoint may lag the status the video was completed by.
//!
//! Only the poller that holds the job's claim records the output or gives it
//! up (both writes are fenced by the claim), and its lease is renewed while
//! it downloads ([`super::fence::BatchFence::renew`]); a poller that learns
//! its claim ended stops downloading. A body the server labels with no video
//! type is stored, and served, as `application/octet-stream`.
use super::{
    content_bounds::{
        Bounded, Cut, VIDEO_DOWNLOAD_IDLE, VIDEO_DOWNLOAD_TOTAL, VIDEO_OUTPUT_MAX_BYTES,
    },
    fence::BatchFence,
    *,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// Failed attempts after which an output that could not be stored is given
/// up (the last attempt's cause is recorded).
pub(crate) const VIDEO_OUTPUT_ATTEMPTS: i32 = 6;
const VIDEO_STORAGE_UNAVAILABLE: &str =
    "Video generation needs the gateway file store with video outputs allowed";
const VIDEO_STORAGE_FULL: &str = "The workspace's storage is full: a finished video is stored in it, so no video is created until space is freed";

/// Why a finished video's output is not stored (the job's
/// `output_failure`, a fixed code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFailure {
    /// The server sent no bytes.
    Empty,
    /// Larger than [`VIDEO_OUTPUT_MAX_BYTES`] (declared or as sent).
    TooLarge,
    /// Retries exhausted: the server refused to send it (a 4xx other than a
    /// rate limit).
    UpstreamRefused,
    /// The file store, or its video outputs, are turned off.
    StorageDisabled,
    /// Retries exhausted: the download failed (unreachable, too slow, cut
    /// off, a 5xx).
    DownloadFailed,
    /// Retries exhausted: the job's route or connection is disabled.
    RouteUnavailable,
    /// Retries exhausted: the file store failed to keep it.
    StorageFailed,
    /// The server gave the video's id to another job
    /// (`upstream_id_reused`): it is never fetched.
    ProviderIdReused,
}
impl OutputFailure {
    pub const ALL: [Self; 8] = [
        Self::Empty,
        Self::TooLarge,
        Self::UpstreamRefused,
        Self::StorageDisabled,
        Self::DownloadFailed,
        Self::RouteUnavailable,
        Self::StorageFailed,
        Self::ProviderIdReused,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty_output",
            Self::TooLarge => "too_large",
            Self::UpstreamRefused => "upstream_refused",
            Self::StorageDisabled => "storage_disabled",
            Self::DownloadFailed => "download_failed",
            Self::RouteUnavailable => "route_unavailable",
            Self::StorageFailed => "storage_failed",
            Self::ProviderIdReused => "upstream_id_reused",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == code)
    }
}

/// One attempt's outcome when the output was not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Try {
    /// A retry cannot change it: recorded now.
    Permanent(OutputFailure),
    /// Retried after the backoff, until [`VIDEO_OUTPUT_ATTEMPTS`].
    Transient(OutputFailure),
}

/// Why a completed video's output will never be available from the gateway:
/// its recorded failure, or `completed_before_storage` for a video completed
/// before the gateway stored outputs (0098: it never awaited one). `None`
/// while the output is stored or still awaited.
pub(crate) fn unavailable_reason(job: &JobRow) -> Option<&'static str> {
    if job.output_file_id.is_some() || job.output_pending {
        return None;
    }
    Some(
        job.output_failure
            .as_deref()
            .and_then(OutputFailure::parse)
            .map_or(COMPLETED_BEFORE_STORAGE, OutputFailure::as_str),
    )
}
/// The reason of a video completed before the gateway stored outputs.
const COMPLETED_BEFORE_STORAGE: &str = "completed_before_storage";
/// Every reason a completed video's output is unavailable: the
/// [`OutputFailure`] codes and [`COMPLETED_BEFORE_STORAGE`].
pub const UNAVAILABLE_REASONS: [&str; OutputFailure::ALL.len() + 1] = {
    let mut reasons = [COMPLETED_BEFORE_STORAGE; OutputFailure::ALL.len() + 1];
    let mut i = 0;
    while i < OutputFailure::ALL.len() {
        reasons[i] = OutputFailure::ALL[i].as_str();
        i += 1;
    }
    reasons
};

impl Jobs {
    /// The finished video is stored in the gateway's file store: without it
    /// (or with video outputs turned off) no job is created, and neither is
    /// one while the workspace's storage, counting [`VIDEO_OUTPUT_MAX_BYTES`]
    /// for each of its completed videos whose output is still awaited, is at
    /// or over its quota. The output is stored without a quota check once the
    /// video is made (it is paid for), so a workspace exceeds its quota by at
    /// most [`VIDEO_OUTPUT_MAX_BYTES`] per video still running (its jobs at
    /// once).
    pub(super) async fn require_video_storage(&self, workspace: Uuid) -> JobResult<()> {
        let unavailable =
            JobError::Conflict("video_storage_unavailable", VIDEO_STORAGE_UNAVAILABLE);
        let files = self.files.as_ref().ok_or(unavailable)?;
        if !files
            .accepts(crate::filestore::Purpose::VideoOutput)
            .await
            .map_err(|_| InferenceError::Storage)?
        {
            return Err(unavailable);
        }
        let usage = files
            .workspace_storage(workspace)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let Some(quota) = usage.quota_bytes else {
            return Ok(());
        };
        let awaiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM async_jobs WHERE output_pending AND workspace_id=$1",
        )
        .bind(workspace)
        .fetch_one(&self.store.pool)
        .await
        .map_err(|_| InferenceError::Storage)?;
        let held = u64::try_from(awaiting)
            .unwrap_or(u64::MAX)
            .saturating_mul(VIDEO_OUTPUT_MAX_BYTES);
        if usage.used_bytes.saturating_add(held) >= quota {
            return Err(JobError::Conflict(
                "storage_quota_exceeded",
                VIDEO_STORAGE_FULL,
            ));
        }
        Ok(())
    }

    /// One attempt to store a completed video's output, by the poller that
    /// holds its claim. A permanent failure, or the last allowed attempt's
    /// failure, is recorded on the job; an earlier transient failure is an
    /// error, so the poller backs off before the next attempt.
    pub(crate) async fn refresh_output(
        &self,
        job: JobRow,
        fence: &BatchFence,
        lost: &AtomicBool,
    ) -> JobResult<JobRow> {
        let failure = match self.store_output(&job, fence, lost).await {
            Ok(()) => None,
            Err(Try::Permanent(f)) => Some(f),
            Err(Try::Transient(f)) if job.poll_failures + 1 >= VIDEO_OUTPUT_ATTEMPTS => Some(f),
            Err(Try::Transient(f)) => {
                tracing::warn!(job_id = %job.id, cause = f.as_str(), "video output not stored; retried after backoff");
                return Err(InferenceError::UpstreamUnavailable.into());
            }
        };
        if let Some(f) = failure {
            tracing::warn!(job_id = %job.id, cause = f.as_str(), "video output given up; the job stays completed and charged");
            store::set_output_failure(&self.store, fence, job.id, f.as_str()).await?;
        }
        Ok(store::reload(&self.store, job.id).await?)
    }

    /// Download the output into the file store (encrypted, at most
    /// [`VIDEO_OUTPUT_MAX_BYTES`], within [`VIDEO_DOWNLOAD_IDLE`] and
    /// [`VIDEO_DOWNLOAD_TOTAL`]) and record it on the job, once. It counts
    /// toward the workspace's storage but never fails on its quota (a video
    /// create is refused at or over the quota instead), and it carries the
    /// job's creator (its key, and that key's holder), so it follows the
    /// workspace's object visibility, legal holds and subject exports as the
    /// job does. The download stops once `lost` says the claim ended.
    async fn store_output(
        &self,
        job: &JobRow,
        fence: &BatchFence,
        lost: &AtomicBool,
    ) -> Result<(), Try> {
        use OutputFailure as F;
        use Try::{Permanent, Transient};
        // Through `Jobs::target` first, which reads the job's
        // `upstream_id_reused` mark in the statement before the call: a
        // marked job is never fetched, and its output is given up.
        let (target, adapter) = self.target(job).await.map_err(|e| match e {
            InferenceError::InvalidUpstream => Permanent(F::ProviderIdReused),
            InferenceError::Storage => Transient(F::StorageFailed),
            _ => Transient(F::RouteUnavailable),
        })?;
        let files = self.files.as_ref().ok_or(Permanent(F::StorageDisabled))?;
        // The storage policy now, not at create: video outputs may have been
        // turned off since.
        if !files
            .accepts(crate::filestore::Purpose::VideoOutput)
            .await
            .map_err(|_| Transient(F::StorageFailed))?
        {
            return Err(Permanent(F::StorageDisabled));
        }
        let upstream = job.upstream().ok_or(Transient(F::DownloadFailed))?;
        let content = call(adapter.video_content(&target, &upstream, VideoAsset::Video))
            .await
            .map_err(|e| match e {
                InferenceError::UpstreamRejected => Transient(F::UpstreamRefused),
                _ => Transient(F::DownloadFailed),
            })?;
        // The wire labels anything but a video type `application/octet-stream`.
        let media = STORED_TYPES
            .iter()
            .copied()
            .find(|t| *t == content.content_type)
            .unwrap_or(GENERIC);
        let creator: Option<Uuid> = match job.user_id {
            Some(user) => Some(user),
            None => sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
                .bind(job.api_key_id)
                .fetch_optional(&self.store.pool)
                .await
                .map_err(|_| Transient(F::StorageFailed))?
                .flatten(),
        };
        match content.content_length {
            Some(0) => return Err(Permanent(F::Empty)),
            Some(n) if n > VIDEO_OUTPUT_MAX_BYTES => return Err(Permanent(F::TooLarge)),
            _ => {}
        }
        let body = Bounded::new(
            content.body,
            VIDEO_OUTPUT_MAX_BYTES,
            VIDEO_DOWNLOAD_IDLE,
            VIDEO_DOWNLOAD_TOTAL,
        );
        let cut = body.cut();
        let new = crate::filestore::NewFile {
            created_by_api_key_id: Some(job.api_key_id),
            created_by_user_id: creator,
            filename: Some(format!(
                "{}.{}",
                client_id(JobKind::Video.prefix(), job.id),
                extension(media)
            )),
            content_type: Some(media.to_owned()),
            max_bytes: Some(VIDEO_OUTPUT_MAX_BYTES),
            quota: crate::filestore::QuotaMode::CountOnly,
            ..crate::filestore::NewFile::internal(
                crate::filestore::Purpose::VideoOutput,
                Some(job.workspace_id),
            )
        };
        // A cut stream fails the write (`Source`): nothing partial is kept.
        // So does a claim that ended meanwhile, at the next chunk (the body's
        // idle limit bounds the wait for one).
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let body = Box::pin(futures_util::StreamExt::map(body, move |chunk| {
            if stopped.load(Ordering::SeqCst) {
                return Err(crate::filestore::FileStoreError::Source);
            }
            chunk.map_err(|_| crate::filestore::FileStoreError::Source)
        }));
        let watch = async {
            while !lost.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            stop.store(true, Ordering::SeqCst);
            // The write then ends on its own, failed.
            std::future::pending().await
        };
        let created = tokio::select! {
            created = files.create(new, body) => created,
            never = watch => never,
        };
        let stored = created.map_err(|e| match cut.get() {
            Some(Cut::TooLarge) => Permanent(F::TooLarge),
            Some(Cut::Empty) => Permanent(F::Empty),
            Some(Cut::Idle | Cut::Total | Cut::Source) => Transient(F::DownloadFailed),
            None => match e {
                crate::filestore::FileError::TooLarge => Permanent(F::TooLarge),
                crate::filestore::FileError::Disabled => Permanent(F::StorageDisabled),
                _ => Transient(F::StorageFailed),
            },
        })?;
        self.publish(job, fence, files, stored.id).await
    }

    /// Record the stored copy `stored` on the job. Not recorded, or not known
    /// to be: the job as it is now decides. A copy it does not hold is
    /// removed; one it may hold (the record's outcome unknown) is kept, at
    /// worst an orphan counted until retention.
    async fn publish(
        &self,
        job: &JobRow,
        fence: &BatchFence,
        files: &crate::filestore::FileStorage,
        stored: Uuid,
    ) -> Result<(), Try> {
        use OutputFailure as F;
        use Try::{Permanent, Transient};
        let published = store::set_video_output(&self.store, fence, job.id, stored).await;
        if matches!(published, Ok(true)) {
            return Ok(());
        }
        let Ok(now) = store::reload(&self.store, job.id).await else {
            tracing::warn!(job_id = %job.id, "video output record outcome unknown; its stored copy is kept");
            return Err(Transient(F::StorageFailed));
        };
        if now.output_file_id == Some(stored) {
            return Ok(());
        }
        let _ = files.delete(stored, Some(job.workspace_id)).await;
        match published {
            // Marked since the download began: what it fetched may be
            // another workspace's video.
            Ok(false) if now.error_code.as_deref() == Some(UPSTREAM_ID_REUSED) => {
                Err(Permanent(F::ProviderIdReused))
            }
            // Deleted meanwhile, or the claim ended.
            Ok(_) => Ok(()),
            Err(_) => Err(Transient(F::StorageFailed)),
        }
    }

    /// The stored output of a completed video, or why there is none.
    pub(crate) async fn stored_output(&self, job: &JobRow) -> JobResult<ContentStream> {
        if let Some(reason) = unavailable_reason(job) {
            return Err(JobError::Gone(
                "video_output_unavailable",
                "The video was generated (and charged), but its output is not stored in the gateway and will not become available",
                Some(reason),
            ));
        }
        let Some(file) = job.output_file_id else {
            return Err(JobError::Conflict(
                "video_content_unavailable",
                "The video's output is not stored yet; try again later",
            ));
        };
        let files = self.files.as_ref().ok_or(InferenceError::Storage)?;
        let (stored, body) = match files.open(file, Some(job.workspace_id)).await {
            Ok(opened) => opened,
            // The job is not deleted, so its copy ended by retention.
            Err(crate::filestore::FileError::NotFound) => {
                return Err(JobError::Gone(
                    "video_output_expired",
                    "The video's stored output has expired",
                    None,
                ));
            }
            Err(_) => return Err(InferenceError::Storage.into()),
        };
        // Only a video output is ever linked to a job (`set_video_output`).
        if stored.purpose != crate::filestore::Purpose::VideoOutput {
            return Err(InferenceError::Storage.into());
        }
        let media = STORED_TYPES
            .iter()
            .copied()
            .find(|t| Some(*t) == stored.content_type.as_deref())
            .unwrap_or(GENERIC);
        Ok(ContentStream {
            content_type: media,
            content_length: Some(stored.size_bytes),
            body: Box::pin(futures_util::StreamExt::map(body, |chunk| {
                chunk.map_err(|_| InferenceError::Storage)
            })),
        })
    }
}

impl Jobs {
    /// Delete a video's stored output (it is unreadable from then on, and
    /// the file sweeper finishes a delete the store could not complete).
    pub(super) async fn delete_output(&self, job: &JobRow, file: Uuid) -> JobResult<()> {
        let files = self.files.as_ref().ok_or(InferenceError::Storage)?;
        files
            .delete(file, Some(job.workspace_id))
            .await
            .map_err(|_| InferenceError::Storage)?;
        Ok(())
    }
}

/// The types a video output is stored and served as: the video types as the
/// server labels them, anything else as [`GENERIC`].
const STORED_TYPES: [&str; 4] = [
    VIDEO_OUTPUT_TYPES[0],
    VIDEO_OUTPUT_TYPES[1],
    VIDEO_OUTPUT_TYPES[2],
    GENERIC,
];
const GENERIC: &str = "application/octet-stream";

/// The stored file's extension for a stored type.
fn extension(media: &str) -> &'static str {
    match media {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ => "bin",
    }
}
