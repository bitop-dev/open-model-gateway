//! Shutdown drain for HTTP/1.1 keep-alive connections.
//!
//! A client reuses an idle keep-alive connection until the server says
//! otherwise. A server that closes an idle connection unannounced can meet
//! the client's next request on the wire, and that request is lost (the
//! client sees the socket close under it). So from the first shutdown signal
//! the gateway keeps serving for a drain window and marks every HTTP/1.x
//! response `Connection: close`: each connection a client uses in the window
//! ends after that response, by the client's own reading. Only when the
//! window ends does the server's graceful shutdown run, which closes the
//! connections still idle (one no client used for the whole window), lets
//! in-flight requests and streams finish, and sends HTTP/2 clients GOAWAY.
//! A second signal ends the window at once.
//!
//! The listener keeps accepting in the window: a connection that arrives
//! from a load balancer's stale endpoint list is answered (and closed after
//! its response) instead of refused.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, Version, header::CONNECTION},
    middleware::Next,
    response::Response,
};

/// Whether the gateway is draining for shutdown. Shared by the middleware
/// ([`mark`]) and [`shutdown`], which sets it.
#[derive(Clone, Default)]
pub struct Drain(Arc<AtomicBool>);

impl Drain {
    fn draining(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Middleware: while draining, an HTTP/1.x response carries
/// `Connection: close`, so the client does not send another request on its
/// connection. HTTP/2 has no such header (its graceful shutdown is GOAWAY),
/// and a protocol switch (`101`, a WebSocket) keeps its own `Connection`.
pub async fn mark(State(drain): State<Drain>, request: Request, next: Next) -> Response {
    let http1 = request.version() <= Version::HTTP_11;
    let mut response = next.run(request).await;
    if http1 && response.status() != StatusCode::SWITCHING_PROTOCOLS && drain.draining() {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// The process's shutdown signals: SIGTERM and Ctrl-C (SIGINT). Registered
/// once, before serving, so a signal during the drain is not missed.
pub struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn register() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                terminate: signal(SignalKind::terminate())?,
                interrupt: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn next(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
        #[cfg(not(unix))]
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Resolves when the server's graceful shutdown should start: at the first
/// signal when `window` is zero, otherwise when the window has passed since
/// that signal or at a second signal. From the first signal on, `drain` is
/// set.
pub async fn shutdown(mut signals: Signals, drain: Drain, window: Duration) {
    signals.next().await;
    tracing::info!("shutdown requested");
    if window.is_zero() {
        return;
    }
    drain.0.store(true, Ordering::Relaxed);
    tracing::info!(
        seconds = window.as_secs(),
        "draining connections before shutdown"
    );
    wait_out(signals, window).await;
}

/// The drain window, or until a second signal.
async fn wait_out(mut signals: Signals, window: Duration) {
    tokio::select! {
        () = tokio::time::sleep(window) => {}
        () = signals.next() => tracing::info!("drain ended by a second shutdown signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use tower::ServiceExt;

    async fn connection(drain: &Drain, path: &str, version: Version) -> Option<String> {
        let app = Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route(
                "/switch",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::SWITCHING_PROTOCOLS)
                        .header(CONNECTION, "upgrade")
                        .body(Body::empty())
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(drain.clone(), mark));
        let request = Request::builder()
            .uri(path)
            .version(version)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        response
            .headers()
            .get(CONNECTION)
            .map(|value| value.to_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn only_draining_http1_responses_are_marked_close() {
        let drain = Drain::default();
        assert_eq!(connection(&drain, "/ok", Version::HTTP_11).await, None);
        drain.0.store(true, Ordering::Relaxed);
        for version in [Version::HTTP_10, Version::HTTP_11] {
            assert_eq!(
                connection(&drain, "/ok", version).await.as_deref(),
                Some("close")
            );
        }
        assert_eq!(connection(&drain, "/ok", Version::HTTP_2).await, None);
        assert_eq!(
            connection(&drain, "/switch", Version::HTTP_11)
                .await
                .as_deref(),
            Some("upgrade")
        );
    }
}
