//! The read deadlines of a large image input body
//! (`inference::image_buffer::BODY_IDLE`, `BODY_MIN_RATE`, `BODY_TOTAL`).
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use axum::body::{Body, Bytes};
use tokio::time::{Instant, Sleep};

use crate::inference::image_buffer::{BODY_RATE_ALLOWANCE, BodyDeadline, ImageInputLimits};

/// A large request body that must keep arriving. Its read fails with
/// [`BodyDeadline`] (408, and the share of the budget goes back) at the first
/// of: `body_total` since the start; no frame for `body_idle`; or the bytes
/// received, plus [`BODY_RATE_ALLOWANCE`], falling short of `body_min_rate`
/// × the time since the start. The floor counts from the start, so a pause
/// spends the slack the body built up and cannot be renewed by a burst.
///
/// One timer stands for the idle bound and the floor: it is always set to
/// the earlier of the two, so its firing is a breach. A frame is counted
/// before the floor is checked, so bytes already waiting are never charged
/// to the client as lateness; the total is checked before the inner body is
/// polled, so a stream of frames cannot postpone it.
pub(super) struct ReadDeadline {
    body: Body,
    limits: ImageInputLimits,
    started: Instant,
    received: usize,
    next: Pin<Box<Sleep>>,
    total: Pin<Box<Sleep>>,
}

impl ReadDeadline {
    pub(super) fn new(body: Body, limits: ImageInputLimits) -> Self {
        let started = Instant::now();
        let mut this = Self {
            body,
            limits,
            started,
            received: 0,
            next: Box::pin(tokio::time::sleep_until(started + limits.body_idle)),
            total: Box::pin(tokio::time::sleep_until(started + limits.body_total)),
        };
        let first = this.deadline_after(started);
        this.next.as_mut().reset(first);
        this
    }
    /// When the floor breaks with no further byte.
    fn floor_at(&self) -> Instant {
        let covered =
            (self.received + BODY_RATE_ALLOWANCE) as f64 / self.limits.body_min_rate.max(1) as f64;
        self.started + Duration::from_secs_f64(covered.min(86_400.0))
    }
    /// The next breach after a frame at `at`: the idle bound or the floor.
    fn deadline_after(&self, at: Instant) -> Instant {
        (at + self.limits.body_idle).min(self.floor_at())
    }
}

impl http_body::Body for ReadDeadline {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let breach = || Poll::Ready(Some(Err(axum::Error::new(BodyDeadline))));
        if this.total.as_mut().poll(cx).is_ready() {
            return breach();
        }
        if let Poll::Ready(frame) = Pin::new(&mut this.body).poll_frame(cx) {
            if let Some(Ok(data)) = frame.as_ref().map(|f| f.as_ref().map(|f| f.data_ref())) {
                this.received += data.map_or(0, Bytes::len);
            }
            let now = Instant::now();
            if frame.as_ref().is_some_and(Result::is_ok)
                && !this.body.is_end_stream()
                && this.floor_at() < now
            {
                return breach();
            }
            let next = this.deadline_after(now);
            this.next.as_mut().reset(next);
            return Poll::Ready(frame);
        }
        if this.next.as_mut().poll(cx).is_ready() {
            return breach();
        }
        Poll::Pending
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::sync::mpsc;

    const KIB: usize = 1024;

    /// Limits with a 64 KiB/s floor (the allowance then covers 2 s), a 10 s
    /// idle bound and a 60 s total.
    fn limits() -> ImageInputLimits {
        ImageInputLimits {
            body_idle: Duration::from_secs(10),
            body_total: Duration::from_secs(60),
            body_min_rate: 64 * KIB,
            ..ImageInputLimits::default()
        }
    }

    type Frames = mpsc::Sender<Result<http_body::Frame<Bytes>, std::io::Error>>;

    /// A body fed by the returned sender, read by a task that reports when
    /// its read ended and whether it was a deadline.
    fn reader(limits: ImageInputLimits) -> (Frames, tokio::task::JoinHandle<(Duration, bool)>) {
        let (tx, rx) = mpsc::channel(16);
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|frame| (frame, rx))
        });
        let body = ReadDeadline::new(Body::new(http_body_util::StreamBody::new(stream)), limits);
        let start = Instant::now();
        let task = tokio::spawn(async move {
            let mut body = body;
            loop {
                match body.frame().await {
                    None => return (start.elapsed(), false),
                    Some(Err(e)) => {
                        let deadline = crate::inference::image_buffer::is_body_deadline(&e);
                        return (start.elapsed(), deadline);
                    }
                    Some(Ok(_)) => {}
                }
            }
        });
        (tx, task)
    }

    fn bytes(n: usize) -> Result<http_body::Frame<Bytes>, std::io::Error> {
        Ok(http_body::Frame::data(Bytes::from(vec![b' '; n])))
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_sends_nothing_fails_when_the_allowance_runs_out() {
        let (_tx, task) = reader(limits());
        let (at, deadline) = task.await.unwrap();
        assert!(deadline);
        // 128 KiB at 64 KiB/s: 2 s, well before the 10 s idle bound.
        assert_eq!(at, Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_spends_the_slack_and_a_burst_does_not_renew_it() {
        let (tx, task) = reader(limits());
        // 192 KiB at once: (192 + 128) KiB covers 5 s from the start.
        tx.send(bytes(192 * KIB)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(4_900)).await;
        // Another 64 KiB at 4.9 s covers one more second: 6 s from the start,
        // not 4.9 s plus a fresh allowance.
        tx.send(bytes(64 * KIB)).await.unwrap();
        let (at, deadline) = task.await.unwrap();
        assert!(deadline);
        assert_eq!(at, Duration::from_secs(6));
    }

    #[tokio::test(start_paused = true)]
    async fn the_idle_bound_fires_when_the_floor_is_far_away() {
        let (tx, task) = reader(limits());
        // 1 MiB covers 18 s of the floor; the 10 s idle bound comes first.
        tx.send(bytes(1024 * KIB)).await.unwrap();
        let (at, deadline) = task.await.unwrap();
        assert!(deadline);
        assert_eq!(at, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_at_the_floor_arrives_in_full_and_the_total_still_ends_it() {
        let (tx, task) = reader(limits());
        for _ in 0..30 {
            tx.send(bytes(64 * KIB)).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        drop(tx);
        let (at, deadline) = task.await.unwrap();
        assert!(!deadline, "a body at the floor arrives");
        assert_eq!(at, Duration::from_secs(30));

        let (tx, task) = reader(ImageInputLimits {
            body_total: Duration::from_secs(5),
            ..limits()
        });
        let feed = tokio::spawn(async move {
            while tx.send(bytes(64 * KIB)).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        let (at, deadline) = task.await.unwrap();
        assert!(deadline);
        assert_eq!(at, Duration::from_secs(5));
        feed.abort();
    }
}
