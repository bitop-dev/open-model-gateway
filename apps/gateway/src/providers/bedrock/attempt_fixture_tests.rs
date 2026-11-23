//! Each response closes its connection, so every additional attempt needs a new socket.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    Unavailable,
    Throttled,
    Disconnect,
    DisconnectAfterRequest,
    Timeout(std::time::Duration),
}
impl Failure {
    pub(crate) fn expected_status(self) -> Option<u16> {
        match self {
            Self::Throttled => Some(429),
            Self::Unavailable => Some(503),
            _ => None,
        }
    }

    pub(crate) async fn check_header_deadline<F: Future>(self, operation: F) -> F::Output {
        if matches!(self, Self::Timeout(_)) {
            // The fixture advances 61 s after headers. The 60 s read must finish
            // before this watchdog, well inside the client's 300 s operation budget.
            tokio::time::timeout(std::time::Duration::from_secs(65), operation)
                .await
                .expect("60 s header deadline did not fire by 65 s")
        } else {
            operation.await
        }
    }
}

use aws_smithy_runtime_api::client::{
    interceptors::{Intercept, context::BeforeDeserializationInterceptorContextRef},
    runtime_components::RuntimeComponents,
};

/// Observe the status at the SDK after transport, before public error mapping.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResponseStatus(Arc<std::sync::Mutex<Option<u16>>>);

impl ResponseStatus {
    pub(crate) fn observed(&self) -> Option<u16> {
        *self.0.lock().unwrap()
    }
}

impl Intercept for ResponseStatus {
    fn name(&self) -> &'static str {
        "ResponseStatus"
    }

    fn read_before_deserialization(
        &self,
        context: &BeforeDeserializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut aws_smithy_types::config_bag::ConfigBag,
    ) -> Result<(), aws_smithy_runtime_api::box_error::BoxError> {
        *self.0.lock().unwrap() = Some(context.response().status().as_u16());
        Ok(())
    }
}

// Separate libtest cases keep every observation visible when a retry cap changes.
macro_rules! attempt_cases {
    ($module:ident, $run:ident, $timeout_seconds:literal $(, $arg:expr)*) => {
        mod $module {
            use super::*;
            #[tokio::test]
            async fn bare_429() { $run(Failure::Throttled $(, $arg)*).await; }
            #[tokio::test]
            async fn bare_503() { $run(Failure::Unavailable $(, $arg)*).await; }
            #[tokio::test]
            async fn disconnect_on_accept() { $run(Failure::Disconnect $(, $arg)*).await; }
            #[tokio::test]
            async fn disconnect_after_request() { $run(Failure::DisconnectAfterRequest $(, $arg)*).await; }
            #[tokio::test]
            async fn headers_withheld() {
                $run(Failure::Timeout(std::time::Duration::from_secs($timeout_seconds)) $(, $arg)*).await;
            }
        }
    };
}
pub(crate) use attempt_cases;

pub(crate) struct Fixture {
    pub(crate) endpoint: String,
    attempts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl Fixture {
    pub(crate) async fn new(failure: Failure, request_prefix: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let received = requests.clone();
        let server = tokio::spawn(async move {
            let mut stalled_connections = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                // Count at accept so a failed connection is visible even before any HTTP headers.
                seen.fetch_add(1, Ordering::SeqCst);
                if matches!(failure, Failure::Disconnect) {
                    drop(socket);
                    continue;
                }
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert!(
                    line.starts_with(request_prefix),
                    "unexpected operation: {line}"
                );
                let mut body_bytes = 0_u64;
                loop {
                    line.clear();
                    assert!(socket.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    assert!(
                        !name.eq_ignore_ascii_case("transfer-encoding"),
                        "fixture expects a sized body"
                    );
                    if name.eq_ignore_ascii_case("content-length") {
                        body_bytes = value.trim().parse().unwrap();
                    }
                }
                let consumed =
                    tokio::io::copy(&mut (&mut socket).take(body_bytes), &mut tokio::io::sink())
                        .await
                        .unwrap();
                assert_eq!(consumed, body_bytes, "incomplete loopback request");
                received.fetch_add(1, Ordering::SeqCst);
                let status = match failure {
                    Failure::Unavailable => "503 Service Unavailable",
                    Failure::Throttled => "429 Too Many Requests",
                    Failure::Timeout(elapsed) => {
                        // Cross the selected deadline only after reading the request headers.
                        stalled_connections.push(socket);
                        tokio::time::pause();
                        tokio::time::advance(elapsed).await;
                        tokio::time::resume();
                        continue;
                    }
                    Failure::DisconnectAfterRequest => continue,
                    Failure::Disconnect => unreachable!(),
                };
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        Self {
            endpoint,
            attempts,
            requests,
            server,
        }
    }

    pub(crate) fn assert_one(&self, failure: Failure) {
        assert!(!self.server.is_finished(), "loopback server failed");
        let expected_requests = usize::from(!matches!(failure, Failure::Disconnect));
        assert_eq!(
            (
                self.attempts.load(Ordering::SeqCst),
                self.requests.load(Ordering::SeqCst)
            ),
            (1, expected_requests),
            "{failure:?}: (accepted sockets, complete requests)"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
