//! `serve`'s pool through a pooler front that answers its first connections
//! as PgBouncer does (the wire bytes of an ErrorResponse with SQLSTATE
//! 08P01), then forwards to the test database; and opened directly.
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

/// When the front refuses a connection: at its login, as a pooler that
/// never logged in to the server, or at its first query, as PgBouncer does
/// once a pool has served (it completes the login from the server
/// parameters it cached).
#[derive(Clone, Copy)]
enum Refuse {
    AtLogin,
    AtFirstQuery,
}

/// One frontend message after the startup phase: its type, length and body.
async fn message(stream: &mut TcpStream) {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).await.unwrap();
    let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    let mut body = vec![0u8; length - 4];
    stream.read_exact(&mut body).await.unwrap();
}

/// A front on 127.0.0.1 that refuses its first `failing` connections with
/// `message` (SQLSTATE 08P01) and forwards every later one to `target`.
async fn front(
    target: (String, u16),
    failing: usize,
    message_text: &'static str,
    refuse: Refuse,
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
                    if let Refuse::AtFirstQuery = refuse {
                        // AuthenticationOk, then ReadyForQuery (idle).
                        client
                            .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'])
                            .await
                            .unwrap();
                        message(&mut client).await;
                    }
                    let _ = client
                        .write_all(&error_response("08P01", message_text))
                        .await;
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
    connections: u32,
    failing: usize,
    message: &'static str,
    window: Duration,
) -> (sqlx::Result<PgPool>, usize, Duration) {
    through_front_refusing(pool, connections, failing, message, window, Refuse::AtLogin).await
}

async fn through_front_refusing(
    pool: &PgPool,
    connections: u32,
    failing: usize,
    message: &'static str,
    window: Duration,
    refuse: Refuse,
) -> (sqlx::Result<PgPool>, usize, Duration) {
    let direct = pool.connect_options();
    let target = (direct.get_host().to_owned(), direct.get_port());
    let (port, seen) = front(target, failing, message, refuse).await;
    let options = (*direct)
        .clone()
        .host("127.0.0.1")
        .port(port)
        .ssl_mode(sqlx::postgres::PgSslMode::Prefer);
    let started = Instant::now();
    let result = connect(
        PgPoolOptions::new()
            .max_connections(connections)
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
    let (result, seen, elapsed) = through_front(&pool, 1, 3, CACHED, WINDOW).await;
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
    let (result, seen, elapsed) = through_front(&pool, 1, 1, WRONG_PASSWORD, WINDOW).await;
    let error = result.expect_err("refused");
    assert!(cached_reason(&error).is_none());
    assert!(matches!(&error, sqlx::Error::Database(e) if e.message() == WRONG_PASSWORD));
    assert_eq!(seen, 1, "one attempt");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

/// A server login that keeps failing fails with the pooler's error once no
/// further attempt would start within the window: a second apart, the
/// attempts start at 0, 1 and 2 s of a 3 s window.
#[sqlx::test(migrations = false)]
async fn a_failure_that_lasts_fails_after_the_window(pool: PgPool) {
    let window = Duration::from_secs(3);
    let (result, seen, elapsed) = through_front(&pool, 1, usize::MAX, CACHED, window).await;
    let error = result.expect_err("still refused after the window");
    assert!(cached_reason(&error).is_some(), "{error}");
    assert_eq!(seen, 3, "attempts in {elapsed:?}");
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < window,
        "{elapsed:?}"
    );
}

/// Every connection of the pool open, idle (each returns to the pool
/// asynchronously after an acquire) and kept open (`min_connections`).
async fn all_open_and_idle(pool: &PgPool, connections: u32) {
    assert_eq!(pool.size(), connections);
    assert_eq!(pool.options().get_min_connections(), connections);
    assert_eq!(
        pool.options().get_idle_timeout(),
        None,
        "never reaped while idle"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.num_idle() < connections as usize {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every connection returned to the pool");
}

/// Directly, `serve`'s whole pool is open and idle before it listens.
#[sqlx::test(migrations = false)]
async fn serve_opens_every_connection_of_its_pool_before_listening(pool: PgPool) {
    let serving = connect(
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(3)),
        (*pool.connect_options()).clone(),
        WINDOW,
    )
    .await
    .unwrap();
    all_open_and_idle(&serving, 4).await;
}

/// The cached failure reaches some of the pool's connections (one of two
/// Pooler instances): the whole open is tried again, keeping those already
/// open, until every connection is open. Every connection that reaches the
/// pooler is one it refused or one the pool keeps: `connections + refused`
/// in all (a fresh pool per attempt would open the kept ones again).
async fn waited_out_until_whole(pool: &PgPool, connections: u32, refused: usize, refuse: Refuse) {
    let (result, seen, elapsed) =
        through_front_refusing(pool, connections, refused, CACHED, WINDOW, refuse).await;
    let serving = result.expect("the whole pool opened once the pooler forwards");
    all_open_and_idle(&serving, connections).await;
    assert_eq!(seen, connections as usize + refused, "connections opened");
    assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&serving)
        .await
        .unwrap();
    assert_eq!(one, 1);
}

/// Four connections, six refused: the first attempt opens none, the second
/// two, the third the last two.
#[sqlx::test(migrations = false)]
async fn a_cached_failure_on_part_of_the_pool_is_waited_out_until_it_is_whole(pool: PgPool) {
    waited_out_until_whole(&pool, 4, 6, Refuse::AtLogin).await;
}

/// The same when the pooler sends the cached failure at each connection's
/// first query, as PgBouncer does once a pool has served: the pool only
/// counts a connection that answered.
#[sqlx::test(migrations = false)]
async fn a_cached_failure_at_the_first_query_is_waited_out_until_the_pool_is_whole(pool: PgPool) {
    waited_out_until_whole(&pool, 4, 6, Refuse::AtFirstQuery).await;
}

/// The chart's pool of 32 (more than `join_all`'s 30, where it collects in
/// order): five refused.
#[sqlx::test(migrations = false)]
async fn a_cached_failure_on_part_of_a_pool_of_32_is_waited_out(pool: PgPool) {
    waited_out_until_whole(&pool, 32, 5, Refuse::AtFirstQuery).await;
}

/// A cached failure that lasts fails the whole pool's open with the
/// pooler's error once no further attempt would start within the window.
#[sqlx::test(migrations = false)]
async fn a_cached_failure_of_the_whole_pool_that_lasts_fails_after_the_window(pool: PgPool) {
    let window = Duration::from_secs(3);
    let (result, _, elapsed) = through_front(&pool, 4, usize::MAX, CACHED, window).await;
    let error = result.expect_err("still refused after the window");
    assert!(cached_reason(&error).is_some(), "{error}");
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < window,
        "{elapsed:?}"
    );
}

/// A cached failure at every first query that lasts fails after the window
/// with the pooler's error.
#[sqlx::test(migrations = false)]
async fn a_cached_failure_at_the_first_query_that_lasts_fails_after_the_window(pool: PgPool) {
    let window = Duration::from_secs(3);
    let (result, _, elapsed) =
        through_front_refusing(&pool, 4, usize::MAX, CACHED, window, Refuse::AtFirstQuery).await;
    let error = result.expect_err("still refused after the window");
    assert!(cached_reason(&error).is_some(), "{error}");
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < window,
        "{elapsed:?}"
    );
}

/// A database with fewer free connection slots than the pool (SQLSTATE
/// 53300, here a role's connection limit) fails startup within the acquire
/// timeout, as a timeout (sqlx retries 53300 until then), which
/// `Config::connect_serve` names `GATEWAY_DATABASE_MAX_CONNECTIONS` for; it
/// is not a pooler's cached failure and is not waited out.
#[sqlx::test(migrations = false)]
async fn a_database_out_of_connection_slots_fails_at_once(pool: PgPool) {
    let role = format!("omg_startup_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'startup-test' CONNECTION LIMIT 2"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let options = (*pool.connect_options())
        .clone()
        .username(&role)
        .password("startup-test");
    let started = Instant::now();
    let result = connect(
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(1)),
        options,
        WINDOW,
    )
    .await;
    let elapsed = started.elapsed();
    sqlx::raw_sql(&format!("DROP ROLE {role}"))
        .execute(&pool)
        .await
        .unwrap();
    let error = result.expect_err("two slots for a pool of four");
    assert!(cached_reason(&error).is_none(), "{error}");
    assert!(matches!(&error, sqlx::Error::PoolTimedOut), "{error:?}");
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
}

/// The logged cause is PgBouncer's cached reason, without its frame; which
/// errors name the pool's setting; and which one startup stops with.
#[test]
fn the_cached_reason_and_the_error_startup_stops_with() {
    struct Pooler(&'static str, &'static str);
    impl std::fmt::Display for Pooler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.1)
        }
    }
    impl std::fmt::Debug for Pooler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.1)
        }
    }
    impl std::error::Error for Pooler {}
    impl sqlx::error::DatabaseError for Pooler {
        fn message(&self) -> &str {
            self.1
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }
    let error = |code, message| sqlx::Error::Database(Box::new(Pooler(code, message)));
    assert_eq!(
        cached_reason(&error("08P01", CACHED)),
        Some("connect failed")
    );
    assert_eq!(
        cached_reason(&error(
            "08P01",
            "server login has been failing, cached error: the database system is shutting down (server_login_retry)"
        )),
        Some("the database system is shutting down")
    );
    assert_eq!(cached_reason(&error("08P01", WRONG_PASSWORD)), None);
    assert_eq!(cached_reason(&error("57P03", CACHED)), None);

    // Out of connection slots names GATEWAY_DATABASE_MAX_CONNECTIONS.
    assert!(out_of_connections(&sqlx::Error::PoolTimedOut));
    assert!(out_of_connections(&error(
        "53300",
        "sorry, too many clients already"
    )));
    assert!(out_of_connections(&error(
        "08P01",
        "no more connections allowed (max_client_conn)"
    )));
    assert!(!out_of_connections(&error("08P01", WRONG_PASSWORD)));
    assert!(!out_of_connections(&error("08P01", CACHED)));

    // A cached failure never hides another error of the same attempt.
    let stopped = |errors| match reported(errors) {
        sqlx::Error::Database(e) => e.message().to_owned(),
        other => other.to_string(),
    };
    assert_eq!(
        stopped(vec![error("08P01", CACHED), error("08P01", WRONG_PASSWORD)]),
        WRONG_PASSWORD
    );
    // A timeout may only be a side effect: the cached cause is reported.
    assert_eq!(
        stopped(vec![sqlx::Error::PoolTimedOut, error("08P01", CACHED)]),
        CACHED
    );
    assert_eq!(stopped(vec![error("08P01", CACHED)]), CACHED);
    assert_eq!(
        stopped(vec![sqlx::Error::PoolTimedOut, error("53300", "too many")]),
        "too many"
    );

    // Tried again only for the cached failure, alone or with timeouts.
    assert_eq!(
        retried(&[error("08P01", CACHED), sqlx::Error::PoolTimedOut]),
        Some("connect failed")
    );
    assert_eq!(
        retried(&[error("08P01", CACHED), error("08P01", WRONG_PASSWORD)]),
        None
    );
    assert_eq!(retried(&[sqlx::Error::PoolTimedOut]), None);
}
