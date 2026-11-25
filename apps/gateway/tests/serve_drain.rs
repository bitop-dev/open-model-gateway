//! What a real `serve` process does with HTTP/1.1 keep-alive connections
//! between SIGTERM and exit. A client reuses an idle keep-alive connection
//! until the server says otherwise, so the server must answer on it with
//! `Connection: close` before it closes it: a connection closed without that
//! can meet the client's next request on the way and lose it.
#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::{ConnectOptions, PgPool};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Child,
    sync::mpsc,
};

/// The drain window the tests configure.
const DRAIN: Duration = Duration::from_secs(3);

struct Serve {
    child: Child,
    lines: mpsc::UnboundedReceiver<String>,
    address: String,
}

impl Serve {
    async fn start(pool: &PgPool, extra: &[(&str, &str)]) -> Self {
        let database = pool.connect_options().to_url_lossy().to_string();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_open-model-gateway"))
            .arg("serve")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("DATABASE_URL", database)
            .env("GATEWAY_ENV_FILE", "/dev/null")
            .env("GATEWAY_ENV", "development")
            .env("GATEWAY_LISTEN", "127.0.0.1:0")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .envs(extra.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (sender, lines) = mpsc::unbounded_channel();
        let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = stderr.next_line().await {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut serve = Self {
            child,
            lines,
            address: String::new(),
        };
        let listening = serve.line("gateway listening").await;
        serve.address = listening
            .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
            .find_map(|word| word.strip_prefix("address="))
            .or_else(|| {
                listening
                    .split('"')
                    .skip_while(|word| *word != "address")
                    .nth(2)
            })
            .unwrap_or_else(|| panic!("no address in {listening}"))
            .to_owned();
        serve
    }

    /// The next stderr line containing `needle` (earlier lines are skipped).
    async fn line(&mut self, needle: &str) -> String {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match self.lines.recv().await {
                    Some(line) if line.contains(needle) => return line,
                    Some(_) => {}
                    None => panic!("serve exited before logging {needle:?}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("serve did not log {needle:?}"))
    }

    fn signal(&self) {
        let pid = self.child.id().unwrap() as libc::pid_t;
        // SAFETY: sends a signal to the child this test started and still owns.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(&self.address).await.unwrap()
    }
}

/// One `GET /health/live` on `stream`: its status line and lowercase header
/// block, or `None` when the server closed the connection without answering.
async fn live(stream: &mut TcpStream) -> Option<(String, String)> {
    stream
        .write_all(b"GET /health/live HTTP/1.1\r\nHost: gateway\r\n\r\n")
        .await
        .ok()?;
    let mut buffer = Vec::new();
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let mut chunk = [0; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                assert!(buffer.is_empty(), "a response cut short: {buffer:?}");
                return None;
            }
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8(buffer[..end].to_vec()).unwrap();
    let (status, headers) = head.split_once("\r\n").unwrap();
    let headers = headers.to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .expect("a sized response")
        .trim()
        .parse()
        .unwrap();
    let mut body = buffer[end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "a body cut short");
        body.extend_from_slice(&chunk[..n]);
    }
    Some((status.to_owned(), headers))
}

fn closes(headers: &str) -> bool {
    headers.lines().any(|line| line == "connection: close")
}

/// The server's end of `stream` is closed: a read sees end of file (or a
/// reset), never data.
async fn closed(stream: &mut TcpStream) -> bool {
    let mut byte = [0; 1];
    matches!(stream.read(&mut byte).await, Ok(0) | Err(_))
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_idle_keep_alive_connection_is_told_to_close_before_it_is_closed(pool: PgPool) {
    let drain = DRAIN.as_secs().to_string();
    let mut serve = Serve::start(&pool, &[("GATEWAY_SHUTDOWN_DRAIN_SECONDS", &drain)]).await;
    let mut reused = serve.connect().await;
    let mut idle = serve.connect().await;
    for stream in [&mut reused, &mut idle] {
        let (status, headers) = live(stream).await.expect("an answer before shutdown");
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(!closes(&headers), "{headers}");
    }

    serve.signal();
    serve.line("shutdown requested").await;
    let signalled = tokio::time::Instant::now();
    // Idle connections stay open while the gateway drains: a client may
    // already be sending its next request on one.
    let mut byte = [0; 1];
    assert!(
        tokio::time::timeout(DRAIN / 3, reused.read(&mut byte))
            .await
            .is_err(),
        "an idle keep-alive connection was closed without a `Connection: close`"
    );
    // A request on it is answered, and told this connection ends here.
    let (status, headers) = live(&mut reused)
        .await
        .expect("a request on a kept-alive connection after SIGTERM is answered");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(closes(&headers), "{headers}");
    assert!(closed(&mut reused).await);
    // A connection still arriving (a stale endpoint list) is served the same.
    let mut late = serve.connect().await;
    let (status, headers) = live(&mut late)
        .await
        .expect("a late connection is answered");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(closes(&headers), "{headers}");

    // One that stays idle all along is closed when the window ends.
    assert!(closed(&mut idle).await);
    let waited = signalled.elapsed();
    assert!(waited >= DRAIN - Duration::from_millis(100), "{waited:?}");
    let status = tokio::time::timeout(Duration::from_secs(30), serve.child.wait())
        .await
        .expect("serve exits after the drain")
        .unwrap();
    assert!(status.success(), "{status:?}");
}

/// A second signal ends the drain at once (an operator's second Ctrl-C).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_second_signal_ends_the_drain(pool: PgPool) {
    let mut serve = Serve::start(&pool, &[("GATEWAY_SHUTDOWN_DRAIN_SECONDS", "60")]).await;
    let mut idle = serve.connect().await;
    assert!(live(&mut idle).await.is_some());
    serve.signal();
    serve.line("shutdown requested").await;
    serve.line("draining").await;
    serve.signal();
    let status = tokio::time::timeout(Duration::from_secs(30), serve.child.wait())
        .await
        .expect("a second signal ends the drain")
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(closed(&mut idle).await);
}
