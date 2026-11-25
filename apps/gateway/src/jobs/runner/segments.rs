//! Finished lines stored in encrypted result segments as they complete.
use super::*;
use crate::jobs::fence::SegmentLines;
#[cfg(test)]
use serde_json::json;
use tokio::time::Instant;

/// serde_json accepts at most 127 nested containers in a public result.
/// The private record adds exactly one object around that result.
const RESULT_DEPTH: usize = 127;
const RECORD_DEPTH: usize = RESULT_DEPTH + 1;

fn result_fits(value: &Value, remaining: usize) -> bool {
    match value {
        Value::Array(values) => {
            remaining > 0 && values.iter().all(|item| result_fits(item, remaining - 1))
        }
        Value::Object(values) => {
            remaining > 0 && values.values().all(|item| result_fits(item, remaining - 1))
        }
        _ => true,
    }
}

fn check_depth(bytes: &[u8]) -> std::io::Result<()> {
    let mut depth = 0;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > RECORD_DEPTH {
                    return Err(std::io::Error::other("result record exceeds depth limit"));
                }
            }
            b'}' | b']' if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    Ok(())
}

/// Parse the envelope without spending the public result's serde depth budget.
pub(super) fn decode_record(bytes: &[u8]) -> Result<(i32, bool, Value), InferenceError> {
    #[derive(serde::Deserialize)]
    struct Envelope<'a> {
        n: i32,
        e: bool,
        #[serde(borrow)]
        r: &'a serde_json::value::RawValue,
    }

    if !bytes.trim_ascii_start().starts_with(b"{") {
        return Err(InferenceError::Storage);
    }
    // RawValue's syntax scanner is iterative. Cap its depth before it
    // buffers nesting, then apply serde's normal validation to the payload.
    check_depth(bytes).map_err(|_| InferenceError::Storage)?;
    let record: Envelope<'_> =
        serde_json::from_slice(bytes).map_err(|_| InferenceError::Storage)?;
    let result = serde_json::from_str(record.r.get()).map_err(|_| InferenceError::Storage)?;
    Ok((record.n, record.e, result))
}

/// A complete JSONL record, including its newline, bounded by the reader limit.
pub(super) struct Record {
    pub(super) line: i32,
    pub(super) execution: Uuid,
    error: bool,
    encoded: Vec<u8>,
}

impl Record {
    pub(super) fn encode(
        line: i32,
        execution: Uuid,
        error: bool,
        value: &Value,
    ) -> Result<Self, serde_json::Error> {
        let encoded = encode_line(line, error, value, RESULT_LINE_BYTES)?;
        Ok(Self {
            line,
            execution,
            error,
            encoded,
        })
    }
}

fn encode_line(
    line: i32,
    error: bool,
    value: &Value,
    limit: usize,
) -> Result<Vec<u8>, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        n: i32,
        e: bool,
        r: &'a Value,
    }
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let needed = self.bytes.len().saturating_add(bytes.len());
            if needed > self.limit {
                return Err(std::io::Error::other("result line exceeds limit"));
            }
            if needed > self.bytes.capacity() {
                let capacity = needed
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(self.limit);
                self.bytes.reserve_exact(capacity - self.bytes.len());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if !result_fits(value, RESULT_DEPTH) {
        return Err(serde_json::Error::io(std::io::Error::other(
            "result exceeds depth limit",
        )));
    }
    let mut out = Bounded {
        bytes: Vec::new(),
        limit: limit.saturating_sub(1),
    };
    serde_json::to_writer(
        &mut out,
        &Envelope {
            n: line,
            e: error,
            r: value,
        },
    )?;
    out.bytes.reserve_exact(1);
    out.bytes.push(b'\n');
    Ok(out.bytes)
}

#[derive(Default)]
struct Segment {
    body: Vec<u8>,
    succeeded: Vec<(i32, Uuid)>,
    failed: Vec<(i32, Uuid)>,
    deadline: Option<Instant>,
}

/// How long a run buffers results before flushing them as a segment.
#[cfg(not(test))]
fn segment_age(_: &RunState) -> Duration {
    SEGMENT_AGE
}
/// [`SEGMENT_AGE`], or the run's test-only age (`Jobs::segment_age`).
#[cfg(test)]
fn segment_age(state: &RunState) -> Duration {
    state.segment_age.unwrap_or(SEGMENT_AGE)
}

impl Segment {
    fn push(&mut self, record: Record, age: Duration) -> bool {
        self.deadline.get_or_insert_with(|| Instant::now() + age);
        if self.body.is_empty() {
            self.body = record.encoded;
        } else {
            self.body.extend(record.encoded);
        }
        if record.error {
            self.failed.push((record.line, record.execution));
        } else {
            self.succeeded.push((record.line, record.execution));
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
        self.record_flush(result, succeeded)
    }

    fn record_flush(&self, result: FenceResult<()>, succeeded: Vec<(i32, Uuid)>) -> bool {
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
                // A single response may exceed the grouping target. Store it alone.
                if segment.body.len() + record.encoded.len() > SEGMENT_BYTES
                    && !writer.flush(&mut segment).await
                {
                    break;
                }
                if segment.push(record, segment_age(&writer.state))
                    && !writer.flush(&mut segment).await
                {
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
#[cfg(all(test, feature = "integration-tests"))]
pub(crate) struct Probe {
    state: Arc<RunState>,
    records: mpsc::Sender<Record>,
    writer: tokio::task::JoinHandle<Result<(), InferenceError>>,
}
#[cfg(all(test, feature = "integration-tests"))]
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
    pub(crate) async fn finished(&self, line: i32, execution: Uuid) {
        let value = json!({"custom_id": line.to_string()});
        self.records
            .send(Record::encode(line, execution, true, &value).unwrap())
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

    #[test]
    fn stored_records_require_named_envelope_fields() {
        let row = json!({"custom_id":"fixture"});
        assert_eq!(
            decode_record(br#"  {"r":{"custom_id":"fixture"},"e":true,"n":7}"#).unwrap(),
            (7, true, row)
        );
        assert!(matches!(
            decode_record(br#"  [7,true,{"custom_id":"fixture"}]"#),
            Err(InferenceError::Storage)
        ));
    }

    #[test]
    fn stored_depth_matches_the_public_result_budget() {
        let value = (0..RESULT_DEPTH).fold(json!(r#"quoted "["{}\]"#), |v, n| {
            if n % 2 == 0 {
                json!([v])
            } else {
                json!({"item":v})
            }
        });
        let record = Record::encode(3, Uuid::nil(), false, &value).unwrap();
        assert!(serde_json::from_slice::<Value>(&record.encoded).is_err());
        assert_eq!(
            decode_record(&record.encoded).unwrap(),
            (3, false, value.clone())
        );

        let too_deep = json!([value]);
        assert!(Record::encode(3, Uuid::nil(), false, &too_deep).is_err());
        let legacy = serde_json::to_vec(&json!({"n":3,"e":false,"r":too_deep})).unwrap();
        assert!(matches!(
            decode_record(&legacy),
            Err(InferenceError::Storage)
        ));
        assert!(matches!(
            decode_record(br#"{"n":3,"e":false,"r":"\uD800"}"#),
            Err(InferenceError::Storage)
        ));
    }

    #[test]
    fn encoding_caps_escaped_json_and_the_newline() {
        let value = json!("\n\n\t");
        let bytes = encode_line(0, false, &value, 128).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(encode_line(0, false, &value, bytes.len()).unwrap(), bytes);
        assert!(encode_line(0, false, &value, bytes.len() - 1).is_err());
    }

    #[test]
    fn oversized_response_is_refused_before_it_reaches_the_writer() {
        let value = json!("\0".repeat(RESULT_LINE_BYTES / 6));
        assert!(Record::encode(0, Uuid::nil(), false, &value).is_err());
    }

    #[test]
    fn a_readable_record_over_the_group_target_flushes_by_itself() {
        let value = json!("x".repeat(SEGMENT_BYTES));
        let record = Record::encode(0, Uuid::nil(), false, &value).unwrap();
        assert!(record.encoded.len() > SEGMENT_BYTES);
        assert!(record.encoded.len() <= RESULT_LINE_BYTES);
        let mut segment = Segment::default();
        assert!(segment.push(record, SEGMENT_AGE));
        assert_eq!(segment.succeeded.len(), 1);
        assert_eq!(segment.body.iter().filter(|&&b| b == b'\n').count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn ready_records_do_not_delay_the_oldest_record_flush() {
        let state = RunState::new(BatchFence {
            job: Uuid::nil(),
            runner: Uuid::nil(),
            epoch: 1,
        });
        let record = |line| Record::encode(line, Uuid::nil(), true, &Value::Null).unwrap();
        let mut segment = Segment::default();
        assert!(!segment.push(record(0), SEGMENT_AGE));
        let (tx, mut rx) = mpsc::channel(1);
        tokio::time::advance(SEGMENT_AGE - Duration::from_millis(1)).await;
        assert!(!segment.push(record(1), SEGMENT_AGE));
        tx.try_send(record(2)).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(
            receive(&mut rx, &state, segment.deadline).await,
            Incoming::Flush
        ));
        assert_eq!(rx.try_recv().unwrap().line, 2);
    }
}
