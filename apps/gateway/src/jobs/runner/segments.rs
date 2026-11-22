//! Finished lines stored in encrypted result segments as they complete.
use super::*;
use crate::jobs::fence::SegmentLines;
use tokio::time::Instant;

#[derive(Default)]
struct Segment {
    body: Vec<u8>,
    succeeded: Vec<(i32, Uuid)>,
    failed: Vec<i32>,
    deadline: Option<Instant>,
}

impl Segment {
    fn push(&mut self, record: Record, encoded: Vec<u8>) -> bool {
        self.deadline
            .get_or_insert_with(|| Instant::now() + SEGMENT_AGE);
        self.body.extend(encoded);
        if let Some(execution) = record.execution {
            self.succeeded.push((record.line, execution));
        } else {
            self.failed.push(record.line);
        }
        self.succeeded.len() + self.failed.len() >= SEGMENT_LINES
            || self.body.len() >= SEGMENT_BYTES
    }
}

struct Writer {
    store: Store,
    files: crate::filestore::FileStorage,
    job: JobRow,
    state: Arc<RunState>,
    part: u32,
}

impl Writer {
    async fn store_segment(&mut self, body: Vec<u8>, lines: SegmentLines<'_>) -> FenceResult<()> {
        let job = &self.job;
        let mut template = io::batch_file(
            true,
            job.workspace_id,
            job.api_key_id,
            job.user_id,
            format!(
                "batch_{}_part_{}_{}.jsonl",
                job.id.simple(),
                self.state.fence.epoch,
                self.part
            ),
        );
        self.part += 1;
        template.max_bytes = Some(body.len() as u64);
        let stream = futures_util::stream::once(async move { Ok(axum::body::Bytes::from(body)) });
        let file = self
            .files
            .create(template, Box::pin(stream))
            .await
            .map_err(|_| FenceError::Storage)?
            .id;
        let recorded = self
            .state
            .fence
            .insert_segment(&self.store, job.workspace_id, file, lines)
            .await;
        if matches!(recorded, Err(FenceError::Stale | FenceError::Storage)) {
            let _ = self.files.delete(file, Some(job.workspace_id)).await;
        }
        recorded.map(|_| ())
    }

    /// A stopped writer drops its receiver so no producer can block on send.
    async fn flush(&mut self, pending: &mut Segment) -> bool {
        if pending.body.is_empty() {
            return true;
        }
        let Segment {
            body,
            succeeded,
            failed,
            ..
        } = std::mem::take(pending);
        let result = if self.state.fenced.load(Ordering::SeqCst) {
            Err(FenceError::Stale)
        } else {
            self.store_segment(
                body,
                SegmentLines {
                    succeeded: &succeeded,
                    failed: &failed,
                },
            )
            .await
        };
        let unknown = matches!(result, Err(FenceError::Unknown));
        match self.state.written(result) {
            Ok(Some(())) => {
                crate::metrics::METRICS.observe_batch_lines(
                    "gateway",
                    &self.job.provider,
                    succeeded.len() as u64,
                    0,
                );
            }
            Ok(None) => {
                for (line, execution) in succeeded {
                    tracing::warn!(execution_id = %execution, line, "batch line outcome not recorded: the runner no longer holds the batch");
                }
                return false;
            }
            Err(_) if unknown || !succeeded.is_empty() => {
                self.state.lost.store(true, Ordering::SeqCst);
                tracing::warn!(job_id = %self.job.id, "batch segment commit not confirmed");
                return false;
            }
            Err(_) => {
                tracing::warn!(job_id = %self.job.id, "batch error segment unavailable");
            }
        }
        true
    }
}

enum Incoming {
    Record(Record),
    Flush,
    Closed,
}

async fn receive(
    records: &mut mpsc::Receiver<Record>,
    state: &RunState,
    deadline: Option<Instant>,
) -> Incoming {
    let age = async {
        match deadline {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        _ = age => Incoming::Flush,
        _ = state.doubt.notified() => Incoming::Flush,
        record = records.recv() => record.map_or(Incoming::Closed, Incoming::Record),
    }
}

/// Group results up to the row, byte or oldest-record age limit. Flush a
/// partial group when dispatch and its in-flight tasks close the channel.
pub(super) async fn write_segments(
    store: Store,
    files: crate::filestore::FileStorage,
    job: JobRow,
    state: Arc<RunState>,
    mut records: mpsc::Receiver<Record>,
) -> Result<(), InferenceError> {
    let mut writer = Writer {
        store,
        files,
        job,
        state,
        part: 0,
    };
    let mut segment = Segment::default();
    loop {
        match receive(&mut records, &writer.state, segment.deadline).await {
            Incoming::Record(record) => {
                let mut bytes = serde_json::to_vec(&json!({
                    "n": record.line, "e": record.execution.is_none(), "r": record.value
                }))
                .map_err(|_| InferenceError::Storage)?;
                bytes.push(b'\n');
                // A single response may exceed the grouping target. Store it alone.
                if segment.body.len() + bytes.len() > SEGMENT_BYTES
                    && !writer.flush(&mut segment).await
                {
                    break;
                }
                if segment.push(record, bytes) && !writer.flush(&mut segment).await {
                    break;
                }
            }
            Incoming::Flush => {
                if !writer.flush(&mut segment).await {
                    break;
                }
            }
            Incoming::Closed => {
                writer.flush(&mut segment).await;
                break;
            }
        }
    }
    Ok(())
}

/// The segment writer of one run, driven by tests.
#[cfg(test)]
pub(crate) struct Probe {
    state: Arc<RunState>,
    records: mpsc::Sender<Record>,
    writer: tokio::task::JoinHandle<Result<(), InferenceError>>,
}
#[cfg(test)]
impl Probe {
    pub(crate) fn start(
        store: Store,
        files: crate::filestore::FileStorage,
        job: JobRow,
        fence: BatchFence,
    ) -> Self {
        let state = Arc::new(RunState::new(fence));
        let (records, receiver) = mpsc::channel(8);
        let writer = tokio::spawn(write_segments(store, files, job, state.clone(), receiver));
        Self {
            state,
            records,
            writer,
        }
    }
    pub(crate) async fn finished(&self, line: i32) {
        let value = json!({"custom_id": line.to_string()});
        self.records
            .send(Record {
                line,
                execution: None,
                value,
            })
            .await
            .unwrap();
    }
    /// The lease is in doubt (a renewal failed).
    pub(crate) fn doubt(&self) {
        self.state.doubt.notify_one();
    }
    pub(crate) fn fenced(&self) -> bool {
        self.state.fenced.load(Ordering::SeqCst)
    }
    pub(crate) async fn close(self) {
        drop(self.records);
        self.writer.await.unwrap().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn ready_records_do_not_delay_the_oldest_record_flush() {
        let state = RunState::new(BatchFence {
            job: Uuid::nil(),
            runner: Uuid::nil(),
            epoch: 1,
        });
        let record = |line| Record {
            line,
            execution: None,
            value: Value::Null,
        };
        let mut segment = Segment::default();
        assert!(!segment.push(record(0), vec![b'a']));
        let (tx, mut rx) = mpsc::channel(1);
        tokio::time::advance(SEGMENT_AGE - Duration::from_millis(1)).await;
        assert!(!segment.push(record(1), vec![b'b']));
        tx.try_send(record(2)).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(
            receive(&mut rx, &state, segment.deadline).await,
            Incoming::Flush
        ));
        assert_eq!(rx.try_recv().unwrap().line, 2);
    }
}
