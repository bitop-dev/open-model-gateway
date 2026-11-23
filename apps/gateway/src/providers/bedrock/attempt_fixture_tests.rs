//! Each response closes its connection, so every additional attempt needs a new socket.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    Unavailable,
    Throttled,
    Disconnect,
    Timeout(std::time::Duration),
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
                loop {
                    line.clear();
                    assert!(socket.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
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
            "{failure:?}: (accepted sockets, complete request headers)"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
