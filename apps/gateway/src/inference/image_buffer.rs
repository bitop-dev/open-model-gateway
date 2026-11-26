//! The per-replica budget of in-flight large request bodies on the image
//! input routes (#24: Chat Completions and Responses).
//!
//! A body up to [`TEXT_BODY_BYTES`] (the shared text limit) needs nothing
//! from the budget; neither does a body without `Content-Length`, which is
//! held to that limit as it streams (413 past it). A larger body must
//! declare its length, at most `body_bytes`, and takes that length, rounded
//! up to whole MiB, from the budget before a byte of it is read; it gives
//! them back when the response body ends (a stream included) or is dropped.
//! With the budget spent the request is answered 503 with `Retry-After` and
//! nothing is read; a body larger than the whole budget is refused outright.
//! So at most `buffer_bytes` of large bodies are in flight per replica, each
//! held in memory by at most [`COPY_FACTOR`] copies (docs/operations.md).
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bodies up to this size are the shared text limit, outside the budget.
pub const TEXT_BODY_BYTES: usize = 2 * 1024 * 1024;
/// The budget's unit: a large body takes its length in whole MiB.
pub const MIB: usize = 1024 * 1024;
/// The largest number of copies of an image's bytes in memory at once
/// (measured; docs/operations.md "Image input memory").
pub const COPY_FACTOR: usize = 3;
/// The largest number of copies of any byte of a large body: the text it
/// may carry beside its images (at most [`TEXT_BODY_BYTES`]) is copied by
/// the parse, each attempt, the upstream value, its serialization and the
/// spliced body. The buffer's worst case counts every byte at this.
pub const WORST_COPIES: usize = 5;
/// `Retry-After` of a request refused because the budget is spent.
pub const BUSY_RETRY_SECONDS: u32 = 1;

/// `GATEWAY_MAX_BODY_BYTES_IMAGE_INPUT` and `GATEWAY_IMAGE_INPUT_BUFFER_BYTES`,
/// and the read deadlines of a large body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInputLimits {
    /// The largest body one request may send.
    pub body_bytes: usize,
    /// In-flight large-body bytes per replica.
    pub buffer_bytes: usize,
    /// A large body's longest pause between frames ([`BODY_IDLE`]).
    pub body_idle: Duration,
    /// A large body's longest time to arrive in full ([`BODY_TOTAL`]).
    pub body_total: Duration,
    /// A large body's slowest average rate after its first `body_idle`
    /// ([`BODY_MIN_RATE`], bytes per second).
    pub body_min_rate: usize,
}

impl Default for ImageInputLimits {
    fn default() -> Self {
        Self {
            body_bytes: 20 * MIB,
            buffer_bytes: DEFAULT_BUFFER_BYTES,
            body_idle: BODY_IDLE,
            body_total: BODY_TOTAL,
            body_min_rate: BODY_MIN_RATE,
        }
    }
}

/// A large body holds its share of the budget only while it keeps arriving:
/// past this pause between frames, below [`BODY_MIN_RATE`] on average from
/// its start (less [`BODY_RATE_ALLOWANCE`]), or past [`BODY_TOTAL`] in all, its read fails
/// (408) and the share goes back. So holding a share costs a client upload
/// bandwidth, not a trickle.
pub const BODY_IDLE: Duration = Duration::from_secs(10);
/// About 1 Mbit/s from the start (after [`BODY_RATE_ALLOWANCE`]): a 20 MiB
/// body may take 160 s.
pub const BODY_MIN_RATE: usize = 128 * 1024;
/// Bytes the rate floor forgives (one second at [`BODY_MIN_RATE`]), so a
/// body's first bytes may arrive just after its headers.
pub const BODY_RATE_ALLOWANCE: usize = 128 * 1024;
/// The longest a large body may take to arrive.
pub const BODY_TOTAL: Duration = Duration::from_secs(180);

/// The default budget. The buffer is a hard bound on large bodies only: at
/// [`WORST_COPIES`] copies its worst case with a [`BASELINE_BYTES`] process
/// (100 + 48 × 5 = 340 MiB) stays within half the chart's 1 GiB memory
/// limit, leaving the rest for text bodies, which no buffer bounds
/// (docs/operations.md "Image input memory"; tests/helm-chart.test.mjs).
pub const DEFAULT_BUFFER_BYTES: usize = 48 * MIB;
/// Memory a running replica holds before any request body (measured warm
/// at about 75 MiB; the allowance in the arithmetic above).
pub const BASELINE_BYTES: usize = 100 * MIB;

impl ImageInputLimits {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let read = |name: &str, default: usize| -> Result<usize> {
            lookup(name).map_or(Ok(default), |v| {
                v.trim()
                    .parse::<usize>()
                    .with_context(|| format!("{name} must be a whole number of bytes"))
            })
        };
        let limits = Self {
            body_bytes: read(
                "GATEWAY_MAX_BODY_BYTES_IMAGE_INPUT",
                Self::default().body_bytes,
            )?,
            buffer_bytes: read(
                "GATEWAY_IMAGE_INPUT_BUFFER_BYTES",
                Self::default().buffer_bytes,
            )?,
            ..Self::default()
        };
        ensure!(
            (TEXT_BODY_BYTES..=256 * MIB).contains(&limits.body_bytes),
            "GATEWAY_MAX_BODY_BYTES_IMAGE_INPUT must be between 2 MiB and 256 MiB"
        );
        ensure!(
            limits.buffer_bytes <= 64 * 1024 * MIB,
            "GATEWAY_IMAGE_INPUT_BUFFER_BYTES must be at most 64 GiB"
        );
        Ok(limits)
    }
}

/// The error of a large body that stopped arriving in time
/// ([`ImageInputLimits::body_idle`], [`ImageInputLimits::body_total`]).
#[derive(Debug)]
pub struct BodyDeadline;
impl std::fmt::Display for BodyDeadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body read deadline")
    }
}
impl std::error::Error for BodyDeadline {}

/// Whether `error`, or an error it wraps, is a [`BodyDeadline`].
pub fn is_body_deadline(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if e.is::<BodyDeadline>() {
            return true;
        }
        current = e.source();
    }
    false
}

/// Marks a request whose declared body is above [`TEXT_BODY_BYTES`]: its
/// declared length. Only image data may take a body past the text limit: a
/// handler refuses it (413) when the bytes outside its images' data URLs
/// exceed [`TEXT_BODY_BYTES`] (text is copied more often than an image,
/// which [`COPY_FACTOR`] assumes).
#[derive(Clone, Copy)]
pub struct LargeBody(pub usize);
impl LargeBody {
    /// Whether this body is within the text limit once `request`'s image
    /// data is set aside.
    pub fn allows(self, request: &crate::inference::types::ChatRequest) -> bool {
        let images: usize = request
            .messages
            .iter()
            .filter_map(|m| m.content.as_ref())
            .flat_map(crate::inference::types::Content::images)
            .map(|image| image.data_url().len())
            .sum();
        images > 0 && self.0.saturating_sub(images) <= TEXT_BODY_BYTES
    }
}

/// A large body's share of the budget, returned when it is dropped or, at
/// the latest, when [`BufferPermit::release_after`]'s time has passed (a
/// response nobody reads must not keep it).
pub struct BufferPermit {
    slot: Arc<std::sync::Mutex<Option<OwnedSemaphorePermit>>>,
    timer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The declared length the body is held to.
    pub length: usize,
}
impl BufferPermit {
    /// Returns the share after `after` even if this value is still alive.
    pub fn release_after(&self, after: Duration) {
        let slot = self.slot.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            drop(slot.lock().unwrap_or_else(|e| e.into_inner()).take());
        });
        *self.timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(timer);
    }
}
impl Drop for BufferPermit {
    fn drop(&mut self) {
        drop(self.slot.lock().unwrap_or_else(|e| e.into_inner()).take());
        if let Some(timer) = self.timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            timer.abort();
        }
    }
}

/// What a request may send, decided from its headers before any body byte.
pub enum Admission {
    /// At most [`TEXT_BODY_BYTES`], streamed or declared.
    Text,
    /// A declared large body, with its share of the budget.
    Large(BufferPermit),
    /// Larger than the route allows, or than the whole budget.
    TooLarge,
    /// The budget is spent for now.
    Busy,
}

/// The budget of one replica (shared by its routes).
#[derive(Clone)]
pub struct ImageBuffer {
    permits: Arc<Semaphore>,
    total_mib: usize,
    limits: ImageInputLimits,
}

impl ImageBuffer {
    pub fn new(limits: ImageInputLimits) -> Self {
        let total_mib = limits.buffer_bytes / MIB;
        Self {
            permits: Arc::new(Semaphore::new(total_mib)),
            total_mib,
            limits,
        }
    }
    pub fn limits(&self) -> ImageInputLimits {
        self.limits
    }
    /// MiB of the budget not taken right now.
    pub fn available_mib(&self) -> usize {
        self.permits.available_permits()
    }
    pub fn admit(&self, content_length: Option<u64>) -> Admission {
        let Some(length) = content_length else {
            return Admission::Text;
        };
        let Ok(length) = usize::try_from(length) else {
            return Admission::TooLarge;
        };
        if length <= TEXT_BODY_BYTES {
            return Admission::Text;
        }
        let mib = length.div_ceil(MIB);
        if length > self.limits.body_bytes || mib > self.total_mib {
            return Admission::TooLarge;
        }
        let Ok(count) = u32::try_from(mib) else {
            return Admission::TooLarge;
        };
        match self.permits.clone().try_acquire_many_owned(count) {
            Ok(permit) => Admission::Large(BufferPermit {
                slot: Arc::new(std::sync::Mutex::new(Some(permit))),
                timer: std::sync::Mutex::new(None),
                length,
            }),
            Err(_) => Admission::Busy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(buffer_mib: usize) -> ImageBuffer {
        ImageBuffer::new(ImageInputLimits {
            body_bytes: 20 * MIB,
            buffer_bytes: buffer_mib * MIB,
            ..ImageInputLimits::default()
        })
    }

    #[test]
    fn text_bodies_and_undeclared_lengths_take_nothing() {
        let b = buffer(4);
        for length in [None, Some(0), Some(TEXT_BODY_BYTES as u64)] {
            assert!(matches!(b.admit(length), Admission::Text));
        }
        assert_eq!(b.available_mib(), 4);
    }

    #[test]
    fn a_large_body_takes_its_length_in_whole_mib_until_dropped() {
        let b = buffer(6);
        let Admission::Large(first) = b.admit(Some(TEXT_BODY_BYTES as u64 + 1)) else {
            panic!("admitted")
        };
        assert_eq!(b.available_mib(), 3, "2 MiB + 1 byte rounds up to 3 MiB");
        assert_eq!(first.length, TEXT_BODY_BYTES + 1);
        let Admission::Large(second) = b.admit(Some(3 * MIB as u64)) else {
            panic!("admitted")
        };
        assert_eq!(b.available_mib(), 0);
        assert!(matches!(b.admit(Some(3 * MIB as u64)), Admission::Busy));
        drop(first);
        assert!(matches!(b.admit(Some(3 * MIB as u64)), Admission::Large(_)));
        drop(second);
        assert_eq!(b.available_mib(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_share_returns_at_its_hold_limit_even_while_its_permit_lives() {
        let b = buffer(4);
        let Admission::Large(permit) = b.admit(Some(3 * MIB as u64)) else {
            panic!("admitted")
        };
        permit.release_after(Duration::from_secs(5));
        assert_eq!(b.available_mib(), 1);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(b.available_mib(), 4, "returned though the permit is alive");
        drop(permit);
        assert_eq!(b.available_mib(), 4, "never returned twice");
    }

    #[test]
    fn bodies_over_the_route_cap_or_the_whole_budget_are_refused_outright() {
        let b = buffer(4);
        assert!(matches!(b.admit(Some(5 * MIB as u64)), Admission::TooLarge));
        assert!(matches!(b.admit(Some(4 * MIB as u64)), Admission::Large(_)));
        let b = buffer(64);
        assert!(matches!(
            b.admit(Some(20 * MIB as u64 + 1)),
            Admission::TooLarge
        ));
        assert!(matches!(b.admit(Some(u64::MAX)), Admission::TooLarge));
        // No budget at all: only text bodies.
        let none = buffer(0);
        assert!(matches!(
            none.admit(Some(3 * MIB as u64)),
            Admission::TooLarge
        ));
        assert!(matches!(none.admit(Some(MIB as u64)), Admission::Text));
    }

    #[test]
    fn settings_are_validated() {
        let limits = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            ImageInputLimits::from_lookup(move |name| {
                pairs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
        };
        assert_eq!(limits(&[]).unwrap(), ImageInputLimits::default());
        let set = limits(&[
            ("GATEWAY_MAX_BODY_BYTES_IMAGE_INPUT", "8388608"),
            ("GATEWAY_IMAGE_INPUT_BUFFER_BYTES", "0"),
        ])
        .unwrap();
        assert_eq!((set.body_bytes, set.buffer_bytes), (8 * MIB, 0));
        for bad in [
            ("GATEWAY_MAX_BODY_BYTES_IMAGE_INPUT", "1"),
            ("GATEWAY_MAX_BODY_BYTES_IMAGE_INPUT", "20MiB"),
            ("GATEWAY_IMAGE_INPUT_BUFFER_BYTES", "-1"),
            ("GATEWAY_IMAGE_INPUT_BUFFER_BYTES", "999999999999999"),
        ] {
            assert!(limits(&[bad]).is_err(), "{bad:?}");
        }
    }
}
