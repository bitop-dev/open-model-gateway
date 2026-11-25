//! `serve`'s first connection through a pooler front that answers its first
//! connections as PgBouncer does (the wire bytes of an ErrorResponse with
//! SQLSTATE 08P01), then forwards to the test database.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const CACHED: &str =
    "server login has been failing, cached error: connect failed (server_login_retry)";
const WRONG_PASSWORD: &str = "password authentication failed";

/// One startup-phase packet: its length (which counts itself), then the body.
async fn packet(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(length) as usize - 4];
    stream.read_exact(&mut body).await.unwrap();
    [length.to_vec(), body].concat()
}

fn error_response(code: &str, message: &str) -> Vec<u8> {
    let mut fields = Vec::new();
    for (kind, value) in [
        (b'S', "FATAL"),
        (b'V', "FATAL"),
        (b'C', code),
        (b'M', message),
    ] {
        fields.push(kind);
        fields.extend_from_slice(value.as_bytes());
        fields.push(0);
    }
    fields.push(0);
    let length = u32::try_from(fields.len() + 4).unwrap().to_be_bytes();
    [vec![b'E'], length.to_vec(), fields].concat()
}

/// A front on 127.0.0.1 that refuses its first `failing` connections with
/// `message` (SQLSTATE 08P01) and forwards every later one to `target`.
async fn front(
    target: (String, u16),
    failing: usize,
    message: &'static str,
) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let target = target.clone();
            tokio::spawn(async move {
                let mut startup = packet(&mut client).await;
                // An SSLRequest (code 80877103) is declined, as a plain-text
                // PgBouncer does, before the startup message.
                if startup.len() == 8 && startup[4..] == 80_877_103u32.to_be_bytes() {
                    client.write_all(b"N").await.unwrap();
                    startup = packet(&mut client).await;
                }
                if n < failing {
                    client
                        .write_all(&error_response("08P01", message))
                        .await
                        .unwrap();
                    return;
                }
                let mut server = TcpStream::connect(target).await.unwrap();
                server.write_all(&startup).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    (port, seen)
}

async fn through_front(
    pool: &PgPool,
    failing: usize,
    message: &'static str,
    window: Duration,
) -> (sqlx::Result<PgPool>, usize, Duration) {
    let direct = pool.connect_options();
    let target = (direct.get_host().to_owned(), direct.get_port());
    let (port, seen) = front(target, failing, message).await;
    let options = (*direct)
        .clone()
        .host("127.0.0.1")
        .port(port)
        .ssl_mode(sqlx::postgres::PgSslMode::Prefer);
    let started = Instant::now();
    let result = connect(
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(3)),
        options,
        window,
    )
    .await;
    (result, seen.load(Ordering::SeqCst), started.elapsed())
}

/// The cached failure passes: `serve` connects once the pooler forwards, on
/// the fourth attempt here, a second apart, and the pool works.
#[sqlx::test(migrations = false)]
async fn a_cached_failed_server_login_is_waited_out(pool: PgPool) {
    let (result, seen, elapsed) = through_front(&pool, 3, CACHED, WINDOW).await;
    let connected = result.expect("connected once the pooler forwards");
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&connected)
        .await
        .unwrap();
    assert_eq!(one, 1);
    assert_eq!(seen, 4, "three refusals, then the connection that works");
    assert!(elapsed >= Duration::from_secs(3), "{elapsed:?}");
}

/// Any other refusal fails at once: PgBouncer reports a wrong password with
/// the same SQLSTATE, and that must not wait.
#[sqlx::test(migrations = false)]
async fn every_other_refusal_fails_at_once(pool: PgPool) {
    let (result, seen, elapsed) = through_front(&pool, 1, WRONG_PASSWORD, WINDOW).await;
    let error = result.expect_err("refused");
    assert!(!pooler_reports_failed_server_login(&error));
    assert!(matches!(&error, sqlx::Error::Database(e) if e.message() == WRONG_PASSWORD));
    assert_eq!(seen, 1, "one attempt");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

/// A server login that keeps failing fails after the window, with the
/// pooler's error.
#[sqlx::test(migrations = false)]
async fn a_failure_that_lasts_fails_after_the_window(pool: PgPool) {
    let window = Duration::from_secs(2);
    let (result, seen, elapsed) = through_front(&pool, usize::MAX, CACHED, window).await;
    let error = result.expect_err("still refused after the window");
    assert!(pooler_reports_failed_server_login(&error), "{error}");
    assert!((2..=4).contains(&seen), "{seen} attempts in {elapsed:?}");
    assert!(
        elapsed >= window && elapsed < window + Duration::from_secs(2),
        "{elapsed:?}"
    );
}
