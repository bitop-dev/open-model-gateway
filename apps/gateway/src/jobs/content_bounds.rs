//! Video content from a provider, held to a size and to read deadlines.
//!
//! `/content` passes a provider's body through. The response head is bounded
//! by the job call's own timeout; the body is bounded here, so that a server
//! that trickles bytes or never ends its body cannot hold the client
//! connection, the upstream connection and the task for ever.
use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::Stream;

use super::types::{ContentStream, VideoAsset};
use crate::inference::error::InferenceError;

/// The largest video passed on from a provider (bytes), the same limit as
/// for a stored output.
pub const VIDEO_OUTPUT_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// A video body that receives nothing for this long is ended.
pub(crate) const VIDEO_DOWNLOAD_IDLE: Duration = Duration::from_secs(60);
/// A video body is ended after this long in all: about 0.35 MB/s for the
/// largest video.
pub(crate) const VIDEO_DOWNLOAD_TOTAL: Duration = Duration::from_secs(25 * 60);
/// A thumbnail or spritesheet: at most this many bytes, with these read
/// deadlines.
pub(crate) const VIDEO_ASSET_MAX_BYTES: u64 = 16 * 1024 * 1024;
const VIDEO_ASSET_IDLE: Duration = Duration::from_secs(30);
const VIDEO_ASSET_TOTAL: Duration = Duration::from_secs(120);

/// Why a [`Bounded`] stream ended early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    TooLarge,
    Empty,
    Idle,
    Total,
    Source,
}

/// A byte stream held to a size and to read deadlines: nothing for `idle`,
/// or longer than `total` in all, ends it, and so does a byte past `max`.
/// An empty stream ends with [`Cut::Empty`]. The reason is kept in `cut`.
pub(crate) struct Bounded<S> {
    inner: S,
    left: u64,
    seen: bool,
    idle: Duration,
    idle_at: Pin<Box<tokio::time::Sleep>>,
    total_at: Pin<Box<tokio::time::Sleep>>,
    cut: Arc<OnceLock<Cut>>,
    done: bool,
}
impl<S> Bounded<S> {
    pub(crate) fn new(inner: S, max: u64, idle: Duration, total: Duration) -> Self {
        Self {
            inner,
            left: max,
            seen: false,
            idle,
            idle_at: Box::pin(tokio::time::sleep(idle)),
            total_at: Box::pin(tokio::time::sleep(total)),
            cut: Arc::new(OnceLock::new()),
            done: false,
        }
    }
    pub(crate) fn cut(&self) -> Arc<OnceLock<Cut>> {
        self.cut.clone()
    }
    fn end(&mut self, cut: Cut) -> Poll<Option<Result<Bytes, Cut>>> {
        self.done = true;
        let _ = self.cut.set(cut);
        Poll::Ready(Some(Err(cut)))
    }
}
impl<S, E> Stream for Bounded<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, Cut>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.total_at.as_mut().poll(cx).is_ready() {
            return self.end(Cut::Total);
        }
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let n = chunk.len() as u64;
                if n > self.left {
                    return self.end(Cut::TooLarge);
                }
                self.left -= n;
                self.seen |= n > 0;
                let next = tokio::time::Instant::now() + self.idle;
                self.idle_at.as_mut().reset(next);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(_))) => self.end(Cut::Source),
            Poll::Ready(None) if !self.seen => self.end(Cut::Empty),
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if self.idle_at.as_mut().poll(cx).is_ready() {
                    return self.end(Cut::Idle);
                }
                Poll::Pending
            }
        }
    }
}

/// A provider's content for `asset`, held to its limit and read deadlines:
/// a video to [`VIDEO_OUTPUT_MAX_BYTES`], [`VIDEO_DOWNLOAD_IDLE`] and
/// [`VIDEO_DOWNLOAD_TOTAL`]; a thumbnail or spritesheet to
/// [`VIDEO_ASSET_MAX_BYTES`] and its own. A declared length over the limit is
/// refused before anything is sent; a deadline ends the body as a timeout,
/// anything else as an invalid upstream answer.
pub(crate) fn bounded_content(
    content: ContentStream,
    asset: VideoAsset,
) -> Result<ContentStream, InferenceError> {
    let (max, idle, total) = match asset {
        VideoAsset::Video => (
            VIDEO_OUTPUT_MAX_BYTES,
            VIDEO_DOWNLOAD_IDLE,
            VIDEO_DOWNLOAD_TOTAL,
        ),
        VideoAsset::Thumbnail | VideoAsset::Spritesheet => {
            (VIDEO_ASSET_MAX_BYTES, VIDEO_ASSET_IDLE, VIDEO_ASSET_TOTAL)
        }
    };
    if content.content_length.is_some_and(|n| n > max) {
        return Err(InferenceError::InvalidUpstream);
    }
    let body = Bounded::new(content.body, max, idle, total);
    Ok(ContentStream {
        content_type: content.content_type,
        content_length: content.content_length,
        body: Box::pin(futures_util::StreamExt::map(body, |chunk| {
            chunk.map_err(|cut| match cut {
                Cut::Idle | Cut::Total => InferenceError::Timeout,
                _ => InferenceError::InvalidUpstream,
            })
        })),
    })
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, stream};

    use super::*;

    fn chunks(sizes: &[usize]) -> Vec<Result<Bytes, InferenceError>> {
        sizes
            .iter()
            .map(|n| Ok(Bytes::from(vec![0u8; *n])))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_body_ends_after_the_idle_time() {
        let inner = stream::iter(chunks(&[4])).chain(stream::pending());
        let mut body = Bounded::new(
            Box::pin(inner),
            100,
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
        let cut = body.cut();
        assert_eq!(body.next().await.unwrap().unwrap().len(), 4);
        let started = tokio::time::Instant::now();
        assert_eq!(body.next().await.unwrap(), Err(Cut::Idle));
        assert_eq!(started.elapsed(), Duration::from_secs(60));
        assert_eq!(cut.get(), Some(&Cut::Idle));
        assert!(body.next().await.is_none(), "ended for good");
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickling_body_ends_at_the_total_deadline() {
        // A byte every 30 s never trips the idle time.
        let inner = stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Some((Ok::<_, InferenceError>(Bytes::from_static(b"x")), ()))
        });
        let mut body = Bounded::new(
            Box::pin(inner),
            u64::MAX,
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
        let started = tokio::time::Instant::now();
        let mut received = 0;
        let end = loop {
            match body.next().await.unwrap() {
                Ok(chunk) => received += chunk.len(),
                Err(cut) => break cut,
            }
        };
        assert_eq!(end, Cut::Total);
        assert_eq!(received, 19);
        assert!(started.elapsed() >= Duration::from_secs(600));
    }

    #[tokio::test]
    async fn an_oversized_or_empty_body_is_cut() {
        let mut body = Bounded::new(
            Box::pin(stream::iter(chunks(&[60, 60]))),
            100,
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
        assert!(body.next().await.unwrap().is_ok());
        assert_eq!(body.next().await.unwrap(), Err(Cut::TooLarge));
        let mut exact = Bounded::new(
            Box::pin(stream::iter(chunks(&[60, 40]))),
            100,
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
        assert!(exact.next().await.unwrap().is_ok());
        assert!(exact.next().await.unwrap().is_ok(), "the limit itself");
        assert!(exact.next().await.is_none());
        let mut empty = Bounded::new(
            Box::pin(stream::iter(chunks(&[]))),
            100,
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
        assert_eq!(empty.next().await.unwrap(), Err(Cut::Empty));
    }

    #[tokio::test(start_paused = true)]
    async fn content_is_refused_over_its_declared_limit_and_cut_as_it_reads() {
        let content = |length: Option<u64>, body: ByteStreamOf| ContentStream {
            content_type: "video/mp4",
            content_length: length,
            body,
        };
        type ByteStreamOf = super::super::types::ByteStream;
        let declared = bounded_content(
            content(
                Some(VIDEO_OUTPUT_MAX_BYTES + 1),
                Box::pin(stream::iter(chunks(&[1]))),
            ),
            VideoAsset::Video,
        );
        assert!(matches!(declared, Err(InferenceError::InvalidUpstream)));
        let thumbnail = bounded_content(
            content(
                Some(VIDEO_ASSET_MAX_BYTES + 1),
                Box::pin(stream::iter(chunks(&[1]))),
            ),
            VideoAsset::Thumbnail,
        );
        assert!(matches!(thumbnail, Err(InferenceError::InvalidUpstream)));
        // An undeclared length is cut once it passes the limit.
        let big = (VIDEO_ASSET_MAX_BYTES as usize) / 2 + 1;
        let mut cut = bounded_content(
            content(None, Box::pin(stream::iter(chunks(&[big, big])))),
            VideoAsset::Thumbnail,
        )
        .unwrap()
        .body;
        assert!(cut.next().await.unwrap().is_ok());
        assert_eq!(
            cut.next().await.unwrap().err(),
            Some(InferenceError::InvalidUpstream)
        );
        // A stall is a timeout.
        let mut stalled = bounded_content(
            content(
                None,
                Box::pin(stream::iter(chunks(&[1])).chain(stream::pending())),
            ),
            VideoAsset::Video,
        )
        .unwrap()
        .body;
        assert!(stalled.next().await.unwrap().is_ok());
        assert_eq!(
            stalled.next().await.unwrap().err(),
            Some(InferenceError::Timeout)
        );
    }
}
