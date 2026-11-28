//! #132: two bounded PostgreSQL 17 regression measurements of scoped
//! admission and settlement under load, on the real HTTP path, engine and a
//! production-shaped `gateway_runtime` pool of 10 (dedicated, ignored probes:
//! they run alone, `node deploy/spark/test-integration.mjs lock-stall`).
//!
//! - [`cold_bucket_admission_contention`]: bursts of concurrent requests on
//!   one workspace and key whose `budget_totals`, minute and in-flight rows do
//!   not exist yet (a period boundary, at its widest: every period at once)
//!   against requests on rows that exist, sent in the same burst and
//!   interleaved, round by round. Every admission locks the
//!   same rows either way (`omg_lock_scope_rows`); the question is whether
//!   creating them adds a stall of its own, and whether one scope's burst
//!   holds the replica's admission gate from another's (#876; also two warm
//!   scopes, one sent 15 ms after the other). It measures how long admissions
//!   take, so it runs with an admission bound longer than any burst; it
//!   prints the server's commit latency, the first burst of the process and,
//!   per burst, how many requests took longer than the production bound.
//! - [`steady_load_recovers_after_a_stall`]: steady open-loop load on one
//!   scope, a held lock on its hot totals row for longer than the settlement
//!   deadline (asserted to hold the row; the requests sent into it must be
//!   shed by the production admission bound), then release: throughput and
//!   latency must return to the pre-stall level within a bounded time, the
//!   settlement gate must drain, and the accounting must stay exact.
//!
//! The database is local, so its commits are fast; the cluster's were slow
//! (synchronous standby, WAL). [`slow_commits`] adds a fixed delay at commit
//! to every transaction that changes an in-flight counter (every admission
//! and settlement), while it still holds its row locks: the cluster's
//! condition, on a disposable database only.
//!
//! The recovery scenario reads `OMG_LOCK_STALL_REPLICAS` (1),
//! `_COMMIT_MS` (5), `_UPSTREAM_MS` (250), `_STALL_MS` (5000), `_RATE`
//! (10 requests per second) and `_MEASURE` (1: print without asserting the
//! shed and recovery bounds; the accounting is still asserted) for
//! measurements off CI (several replicas, each
//! with its own pool of 10 and gates, share the scope); unset, it runs the
//! CI configuration.
//!
//! Numbers are printed (`lock_stall ...` lines, no identifiers or bodies);
//! the asserted bounds are wide multiples of the measured baseline, so they
//! hold on a loaded CI runner and fail only on a stall or a collapse.
use super::{tests::db::*, *};
use crate::{
    auth::NewApiKey,
    inference::{Engine, EngineLimits},
    providers::{
        ProviderRegistry,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::{ConnectOptions, PgPool, postgres::PgPoolOptions};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const GUARD: Duration = Duration::from_secs(60);
const APP: &str = "omg_lock_stall_runtime";
/// `Config::connect`'s default; the gates are derived from it.
const POOL_SIZE: u32 = 10;

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("server-provider-key".into())
    }
}

struct Harness {
    /// The fixture's user (owner of every new scope) and model.
    user: Uuid,
    model: Uuid,
    owner: PgPool,
    runtime: PgPool,
    observer: PgPool,
    store: Store,
    url: String,
    /// Every replica's store and URL (the first is `store` and `url`).
    replicas: Vec<(Store, PgPool, String)>,
    client: reqwest::Client,
    calls: Arc<AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// What the scenario needs from the test runtime's setup.
#[derive(Clone)]
struct Setup {
    options: sqlx::postgres::PgConnectOptions,
    user: Uuid,
    model: Uuid,
}

/// On the test's runtime: the runtime role, its grants (applied and
/// verified, never reduced), the fixture and a price. Returns the setup and
/// the session holding the database-scoped role lock (the dedicated command
/// runs this probe alone on its cluster).
async fn setup(owner: &PgPool) -> (Setup, sqlx::pool::PoolConnection<sqlx::Postgres>) {
    let mut role_lock = owner.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(72419505)")
        .execute(&mut *role_lock)
        .await
        .unwrap();
    sqlx::raw_sql(concat!(include_str!("../../tests/common/ensure_runtime_role.sql"), "REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime; REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime;"))
        .execute(&mut *role_lock).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *role_lock)
        .await
        .unwrap();
    let database = database.replace('"', "\"\"");
    sqlx::raw_sql(&format!("REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE \"{database}\" TO gateway_runtime"))
        .execute(&mut *role_lock).await.unwrap();
    let owner_store = Store::new(owner.clone());
    owner_store.apply_runtime_grants().await.unwrap();
    owner_store.verify_runtime_grants().await.unwrap();
    let f = fixture(owner.clone()).await;
    f.price(1_000_000).await;
    // No limit of its own: the load is bounded by the offered rate.
    sqlx::raw_sql("UPDATE workspace_type_policies SET requests_per_minute=NULL,tokens_per_minute=NULL,concurrent_requests=NULL")
        .execute(owner)
        .await
        .unwrap();
    let setup = Setup {
        options: (*owner.connect_options()).clone(),
        user: f.owner,
        model: f.model,
    };
    (setup, role_lock)
}

/// Run `scenario` on a multi-threaded runtime of its own, as `serve` runs
/// (the test runtime has one thread: the gateway, its upstream and the load
/// would share it, and every transaction would hold its locks while the
/// thread works on something else). Its panic is the test's.
async fn on_multi_thread(scenario: impl Future<Output = ()> + Send + 'static) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.spawn(scenario).await;
    runtime.shutdown_background();
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

/// A runtime pool of 10 as `serve` opens one: all connections open before
/// any load (`db::startup::connect`), unless the measurement asks for the
/// pool before #132, which opens connections on demand
/// (`OMG_LOCK_STALL_COLD_POOLS`).
async fn runtime_pool(options: &sqlx::postgres::PgConnectOptions) -> PgPool {
    let pool = PgPoolOptions::new()
        .max_connections(POOL_SIZE)
        .acquire_timeout(crate::config::ACQUIRE_TIMEOUT)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::Executor::execute(c, "SET ROLE gateway_runtime").await?;
                Ok(())
            })
        });
    let options = options
        .clone()
        .application_name(APP)
        .disable_statement_logging();
    if std::env::var("OMG_LOCK_STALL_COLD_POOLS").as_deref() == Ok("1") {
        pool.connect_with(options).await.unwrap()
    } else {
        crate::db::startup::connect(pool, options, std::time::Duration::ZERO)
            .await
            .unwrap()
    }
}

/// On the scenario's runtime: an owner pool, the runtime pool of 10, an
/// HTTP gateway on it and a mock OpenAI upstream that answers after
/// `upstream_ms`.
///
/// `admission_lock_wait`: the production bound
/// ([`crate::governance::ADMISSION_LOCK_WAIT`]) for a probe of the bound, or
/// a longer one for a probe that measures how long admissions take.
async fn harness(
    setup: Setup,
    upstream_ms: u64,
    replicas: usize,
    admission_lock_wait: Duration,
) -> Harness {
    let options = setup.options;
    let owner = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.clone().disable_statement_logging())
        .await
        .unwrap();
    let runtime = runtime_pool(&options).await;
    // Production sessions have no PostgreSQL deadline: only the gateway's.
    let deadlines: (String, String, String) = sqlx::query_as(
        "SELECT current_user,current_setting('lock_timeout'),current_setting('statement_timeout')",
    )
    .fetch_one(&runtime)
    .await
    .unwrap();
    assert_eq!(
        deadlines,
        ("gateway_runtime".into(), "0".into(), "0".into())
    );
    let observer = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            options
                .clone()
                .application_name("omg_lock_stall_observer")
                .disable_statement_logging(),
        )
        .await
        .unwrap();
    let store = Store::new(runtime.clone()).with_admission_lock_wait(admission_lock_wait);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let upstream = Router::new().fallback(move |Json(_): Json<Value>| {
        let calls = counted.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(upstream_ms)).await;
            Json(json!({"id":"upstream","object":"chat.completion","created":0,"model":"mock-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}))
        }
    });
    let mut tasks = Vec::new();
    let mock = serve(upstream, &mut tasks).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{mock}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(
        Arc::new(store.clone()),
        registry.clone(),
        EngineLimits::default(),
    )
    .unwrap();
    let base = serve(
        crate::http::router_with_engine(store.clone(), None, engine),
        &mut tasks,
    )
    .await;
    let mut all = vec![(
        store.clone(),
        runtime.clone(),
        format!("{base}/v1/chat/completions"),
    )];
    // More replicas: each its own pool of 10, gates and engine, as `serve`.
    for _ in 1..replicas {
        let pool = runtime_pool(&options).await;
        let replica = Store::new(pool.clone()).with_admission_lock_wait(admission_lock_wait);
        let engine = Engine::new(
            Arc::new(replica.clone()),
            registry.clone(),
            EngineLimits::default(),
        )
        .unwrap();
        let base = serve(
            crate::http::router_with_engine(replica.clone(), None, engine),
            &mut tasks,
        )
        .await;
        all.push((replica, pool, format!("{base}/v1/chat/completions")));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(GUARD)
        .build()
        .unwrap();
    Harness {
        user: setup.user,
        model: setup.model,
        owner,
        runtime,
        observer,
        store,
        url: format!("{base}/v1/chat/completions"),
        replicas: all,
        client,
        calls,
        tasks,
    }
}

async fn serve(app: Router, tasks: &mut Vec<tokio::task::JoinHandle<()>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    base
}

/// A new team workspace (with the model) and a key in it: every totals,
/// minute and in-flight row of both scopes is still missing. Returns the
/// workspace and the key's token.
async fn new_scope(h: &Harness) -> (Uuid, String) {
    let ws = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,$2,'team')")
        .bind(ws)
        .bind(format!("lock stall {ws}"))
        .execute(&h.owner)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'owner','manual')")
        .bind(ws).bind(h.user).execute(&h.owner).await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(ws)
    .bind(h.model)
    .execute(&h.owner)
    .await
    .unwrap();
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'lock stall',$4)")
        .bind(key.id).bind(ws).bind(h.user).bind(key.digest.as_slice()).execute(&h.owner).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM budget_totals WHERE scope_id=$1)+(SELECT count(*) FROM inflight_counters WHERE scope_id=$1)")
        .bind(ws).fetch_one(&h.owner).await.unwrap();
    assert_eq!(rows, 0, "the new scope's rows do not exist yet");
    (ws, key.token)
}

/// A fixed `ms` delay at commit of every transaction that changes an
/// in-flight counter, while it holds its locks (a deferred constraint
/// trigger, once per transaction). Test databases only.
async fn slow_commits(owner: &PgPool, ms: u32) {
    sqlx::raw_sql(&format!(
        "CREATE SEQUENCE omg_test_slow_commits;
         GRANT USAGE ON SEQUENCE omg_test_slow_commits TO gateway_runtime;
         CREATE FUNCTION omg_test_slow_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
          IF current_setting('omg_test.slow_commit', true) IS DISTINCT FROM 'done' THEN
           PERFORM set_config('omg_test.slow_commit','done',true);
           PERFORM nextval('omg_test_slow_commits');
           PERFORM pg_sleep({ms}/1000.0);
          END IF;
          RETURN NULL;
         END $$;
         CREATE CONSTRAINT TRIGGER omg_test_slow_commit AFTER INSERT OR UPDATE ON inflight_counters
          DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_test_slow_commit();"
    ))
    .execute(owner)
    .await
    .unwrap();
}

#[derive(Clone, Copy, Debug)]
struct Sent {
    /// Since the start of the run.
    at: Duration,
    latency: Duration,
    status: u16,
    accounting_unavailable: bool,
    /// Shed before admission (`scope_busy`), and its `Retry-After`.
    shed: bool,
    retry_after: Option<u32>,
}

async fn send(client: reqwest::Client, url: String, key: String, start: Instant) -> Sent {
    let sent = Instant::now();
    let response = client
        .post(url)
        .bearer_auth(key)
        .json(&json!({"model":"company/smart","max_completion_tokens":10,
            "messages":[{"role":"user","content":"lock stall"}]}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok()?.parse().ok());
    let body = response.text().await.unwrap_or_default();
    Sent {
        at: sent - start,
        latency: sent.elapsed(),
        status,
        accounting_unavailable: body.contains("accounting_unavailable"),
        shed: status == 503 && body.contains("\"scope_busy\""),
        retry_after,
    }
}

fn quantile(latencies: &mut [Duration], q: f64) -> Duration {
    if latencies.is_empty() {
        return Duration::ZERO;
    }
    latencies.sort();
    let i = ((latencies.len() as f64 - 1.0) * q).round() as usize;
    latencies[i]
}
fn ms(d: Duration) -> u128 {
    d.as_millis()
}

/// Lock waiters among the runtime's sessions, sampled while `done` is
/// unset: (most at once, samples with a waiter at the scope-row lock
/// function, samples with any waiter, samples). Never reads query text out.
async fn sample_waits(
    observer: PgPool,
    done: Arc<std::sync::atomic::AtomicBool>,
) -> (i64, i64, i64, i64) {
    let (mut most, mut at_rows, mut any, mut samples) = (0, 0, 0, 0);
    while !done.load(Ordering::SeqCst) {
        let (waiting, rows): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE wait_event_type='Lock'),
               count(*) FILTER (WHERE wait_event_type='Lock' AND query LIKE 'SELECT omg_lock_scope_rows(%')
             FROM pg_stat_activity WHERE datname=current_database() AND application_name=$1",
        )
        .bind(APP)
        .fetch_one(&observer)
        .await
        .unwrap();
        samples += 1;
        most = most.max(waiting);
        any += i64::from(waiting > 0);
        at_rows += i64::from(rows > 0);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    (most, at_rows, any, samples)
}

/// `n` requests at once with `key`: every response, and the lock waits
/// sampled meanwhile.
async fn send_burst(h: &Harness, key: &str, n: usize) -> (Vec<Sent>, (i64, i64, i64, i64)) {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = tokio::spawn(sample_waits(h.observer.clone(), done.clone()));
    let start = Instant::now();
    let mut sends = Vec::new();
    for _ in 0..n {
        sends.push(tokio::spawn(send(
            h.client.clone(),
            h.url.clone(),
            key.to_owned(),
            start,
        )));
    }
    let mut sent = Vec::new();
    for s in sends {
        sent.push(tokio::time::timeout(GUARD, s).await.unwrap().unwrap());
    }
    done.store(true, Ordering::SeqCst);
    (sent, sampler.await.unwrap())
}

/// `n` requests at once with `key`; their latencies, after asserting each
/// succeeded.
async fn burst(h: &Harness, key: &str, n: usize) -> (Vec<Duration>, (i64, i64, i64, i64)) {
    let (sent, waits) = send_burst(h, key, n).await;
    let mut latencies = Vec::new();
    for s in sent {
        assert_eq!(s.status, 200, "{s:?}");
        latencies.push(s.latency);
    }
    (latencies, waits)
}

/// `n` requests with each of `first` and `second` at once, sent alternately
/// (so both sides meet the same server, pool and runner), the `second` side's
/// each `delay` after its `first` partner; each side's latencies (each from
/// its own send) after asserting each request succeeded, and the lock waits
/// sampled meanwhile (both sides together).
async fn paired_burst(
    h: &Harness,
    first: &str,
    second: &str,
    n: usize,
    delay: Duration,
) -> (Vec<Duration>, Vec<Duration>, (i64, i64, i64, i64)) {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = tokio::spawn(sample_waits(h.observer.clone(), done.clone()));
    let start = Instant::now();
    let mut sends = Vec::with_capacity(2 * n);
    for _ in 0..n {
        for (key, wait) in [(first, Duration::ZERO), (second, delay)] {
            let (client, url, key) = (h.client.clone(), h.url.clone(), key.to_owned());
            sends.push(tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                send(client, url, key, start).await
            }));
        }
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for (i, s) in sends.into_iter().enumerate() {
        let s = tokio::time::timeout(GUARD, s).await.unwrap().unwrap();
        assert_eq!(s.status, 200, "{s:?}");
        if i % 2 == 0 {
            a.push(s.latency);
        } else {
            b.push(s.latency);
        }
    }
    done.store(true, Ordering::SeqCst);
    (a, b, sampler.await.unwrap())
}

/// A measured burst, never asserted: the latencies of the requests that
/// succeeded (the printed quantiles and `latency_over_admission_bound` are
/// theirs), and how many did not, by outcome (`scope_busy`,
/// `accounting_unavailable` or the HTTP status). Their accounting is still
/// checked afterwards ([`assert_exact`]).
async fn measured_burst(
    h: &Harness,
    key: &str,
    n: usize,
) -> (Vec<Duration>, std::collections::BTreeMap<String, usize>) {
    let (sent, _) = send_burst(h, key, n).await;
    let mut latencies = Vec::new();
    let mut failed = std::collections::BTreeMap::new();
    for s in sent {
        if s.status == 200 {
            latencies.push(s.latency);
            continue;
        }
        let outcome = if s.shed {
            "scope_busy".to_owned()
        } else if s.accounting_unavailable {
            "accounting_unavailable".to_owned()
        } else {
            format!("status_{}", s.status)
        };
        *failed.entry(outcome).or_insert(0) += 1;
    }
    (latencies, failed)
}

/// Accounting after a run: nothing pending once the settlements that did
/// not finish are reconciled at lease expiry (unknown, hold kept), no
/// execution settled twice, one upstream call per reservation (no replay),
/// in-flight counters back to zero, and `budget verify` and `history verify`
/// clean. `others` are further gateways on the same database: their replicas'
/// gates must have drained too, and their upstream calls count with `h`'s.
/// Every request has been answered, so every gate permit is back: a
/// settlement past its deadline (`accounting_unavailable`) was dropped with
/// its permit and its transaction, which never commits, before that answer.
/// Returns (settled, unknown).
async fn assert_exact(h: &Harness, others: &[&Harness]) -> (i64, i64) {
    // The settlement gate drained: every permit is back.
    let expected = LockGates::for_pool(POOL_SIZE)
        .settlement
        .available_permits();
    let gateways = || std::iter::once(h).chain(others.iter().copied());
    for (store, _, _) in gateways().flat_map(|g| &g.replicas) {
        assert_eq!(
            store.lock_gates.settlement.available_permits(),
            expected,
            "every replica's settlement gate drained"
        );
    }
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE state='pending'")
        .execute(&h.owner)
        .await
        .unwrap();
    while reconcile_expired(&h.store, 500).await.unwrap() > 0 {}
    let (pending, settled, unknown, reservations): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state='pending'),count(*) FILTER (WHERE state='settled'),count(*) FILTER (WHERE state='unknown'),count(*) FROM governance_reservations",
    )
    .fetch_one(&h.owner)
    .await
    .unwrap();
    assert_eq!(pending, 0, "nothing stays pending");
    let twice: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT execution_id FROM monetary_ledger WHERE kind IN ('settlement','reconciliation') GROUP BY execution_id HAVING count(*)>1) d")
        .fetch_one(&h.owner).await.unwrap();
    assert_eq!(twice, 0, "no execution settled twice");
    let unknown_settled: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE state='unknown' AND actual_microusd IS NOT NULL")
        .fetch_one(&h.owner).await.unwrap();
    assert_eq!(unknown_settled, 0, "an unknown outcome is never valued");
    let calls: usize = gateways().map(|g| g.calls.load(Ordering::SeqCst)).sum();
    assert_eq!(
        calls as i64, reservations,
        "one upstream call per reservation: nothing replayed"
    );
    let inflight: i64 =
        sqlx::query_scalar("SELECT coalesce(sum(requests+jobs),0)::bigint FROM inflight_counters")
            .fetch_one(&h.owner)
            .await
            .unwrap();
    assert_eq!(inflight, 0, "in-flight counters drained");
    let totals = totals::verify(&h.store).await.unwrap();
    assert!(totals.consistent(), "budget verify: {totals:?}");
    let history = crate::history::verify(&h.runtime, None).await.unwrap();
    assert!(history.consistent(), "history verify");
    // Process-global pressure samples belong to this database.
    pressure::flush(&h.store).await.unwrap();
    (settled, unknown)
}

const BURST: usize = 40;
const ROUNDS: usize = 5;

#[dbtest::test]
#[ignore = "dedicated PostgreSQL runtime-role lock-stall measurement (#132)"]
async fn cold_bucket_admission_contention(owner: PgPool) {
    let (setup, _role_lock) = setup(&owner).await;
    on_multi_thread(cold_buckets(setup)).await;
}
async fn cold_buckets(setup: Setup) {
    // A probe of how long admissions on missing and existing rows take, so
    // with a bound longer than any burst (its orchestration guard): the
    // production bound would cut the slowest latencies off at 1.5 s and hide
    // what is compared. It therefore does not show whether the production
    // bound would shed a burst; how many requests took longer than that
    // bound is printed per burst instead, a measurement of the runner.
    let again = setup.clone();
    let h = harness(setup, 0, 1, GUARD).await;
    commit_latency(&h.owner).await;
    // A discarded first burst on a scope of its own: the first burst of a
    // process also opens connections and prepares statements.
    let (_, first) = new_scope(&h).await;
    let (mut latencies, _) = burst(&h, &first, BURST).await;
    eprintln!(
        "lock_stall cold_bucket round=first requests={BURST} p50_ms={} max_ms={} latency_over_admission_bound={}",
        ms(quantile(&mut latencies, 0.5)),
        ms(quantile(&mut latencies, 1.0)),
        over_bound(&latencies)
    );
    let (_, warm_key) = new_scope(&h).await;
    // Warm: its rows exist after one request.
    burst(&h, &warm_key, 1).await;
    // Each round sends a burst on a new scope (cold: none of its rows exist)
    // and one on the warm scope together, their requests interleaved, so
    // whatever slows the runner or the server during the round slows both
    // alike; each round compares its two sides. A cold-bucket stall would
    // show as the cold side well slower than the warm side of its own round.
    // So would a burst of one scope holding the replica's admission gate
    // while the other's wait (#876): admissions of one workspace run one at
    // a time past its row locks, and the cold side reaches the gate slightly
    // later (its first requests load the workspace's guardrail policy).
    // Then the same with two warm scopes, the second sent 15 ms after the
    // first: a scope that arrives a little later waits its own turn, not
    // for the other's burst. Every round must pass, so a stall that shows in
    // one round only still fails.
    let bound = |earlier: Duration| earlier * 3 / 2 + Duration::from_millis(50);
    let (_, later_key) = new_scope(&h).await;
    burst(&h, &later_key, 1).await;
    let mut stalls = Vec::new();
    for (label, delay) in [
        ("cold", Duration::ZERO),
        ("later", Duration::from_millis(15)),
    ] {
        for round in 0..ROUNDS {
            let (first, second) = if delay.is_zero() {
                (new_scope(&h).await.1, warm_key.clone())
            } else {
                (warm_key.clone(), later_key.clone())
            };
            // `slow` is the side expected to be the slower one: the cold
            // scope, or the scope sent later.
            let (mut a, mut b, (most, at_rows, any, samples)) =
                paired_burst(&h, &first, &second, BURST, delay).await;
            let (slow, other) = if delay.is_zero() {
                (&mut a, &mut b)
            } else {
                (&mut b, &mut a)
            };
            let (slow_p50, slow_max) = (quantile(slow, 0.5), quantile(slow, 1.0));
            let (other_p50, other_max) = (quantile(other, 0.5), quantile(other, 1.0));
            eprintln!(
                "lock_stall cold_bucket {label} round={round} requests={BURST}+{BURST} {label}_p50_ms={} {label}_max_ms={} other_p50_ms={} other_max_ms={} {label}_over_admission_bound={} other_over_admission_bound={} most_lock_waiters={most} samples_waiting_at_scope_rows={at_rows} samples_waiting={any} samples={samples}",
                ms(slow_p50),
                ms(slow_max),
                ms(other_p50),
                ms(other_max),
                over_bound(slow),
                over_bound(other)
            );
            if slow_p50 > bound(other_p50) || slow_max > bound(other_max) {
                stalls.push(format!(
                    "{label} round {round}: p50 {}ms vs {}ms, max {}ms vs {}ms",
                    ms(slow_p50),
                    ms(other_p50),
                    ms(slow_max),
                    ms(other_max)
                ));
            }
        }
    }
    assert!(
        stalls.is_empty(),
        "admissions of one scope wait beyond another's in the same burst: {stalls:?}"
    );
    let (settled, unknown) = assert_exact(&h, &[]).await;
    assert_eq!(
        (settled, unknown),
        ((BURST + 2 + 4 * ROUNDS * BURST) as i64, 0),
        "every burst request settled"
    );
    // A measurement, never asserted: a new gateway on the same database (its
    // own pool, store and engine, as a restarted replica on a direct
    // connection), its first burst and a later one, each on a scope of its
    // own. A new backend's first executions plan and run slower, inside the
    // section where one scope's admissions serialize, so the first burst is
    // slower; how many requests took longer than the production bound is
    // printed with it. A request that does not succeed (a settlement past
    // its deadline answers `accounting_unavailable`) is counted and printed,
    // not failed; the accounting of every request of both gateways is then
    // checked exactly as above.
    let fresh = harness(again, 0, 1, GUARD).await;
    for label in ["new_gateway_first", "new_gateway_later"] {
        let (_, key) = new_scope(&fresh).await;
        let (mut latencies, failed) = measured_burst(&fresh, &key, BURST).await;
        eprintln!(
            "lock_stall cold_bucket round={label} requests={BURST} ok={} failed={failed:?} p50_ms={} max_ms={} latency_over_admission_bound={}",
            latencies.len(),
            ms(quantile(&mut latencies, 0.5)),
            ms(quantile(&mut latencies, 1.0)),
            over_bound(&latencies)
        );
    }
    let (settled, unknown) = assert_exact(&h, &[&fresh]).await;
    eprintln!("lock_stall cold_bucket new_gateway settled={settled} unknown={unknown}");
}

/// Requests of a burst whose whole latency exceeded the production admission
/// bound. Their admission wait (from the admission gate to the row locks) is
/// part of that latency, so this is an upper bound on how many the bound
/// would have shed (`scope_busy`) on this runner.
fn over_bound(latencies: &[Duration]) -> usize {
    latencies
        .iter()
        .filter(|l| **l > ADMISSION_LOCK_WAIT)
        .count()
}

/// The server's commit latency, measured before the bursts: single-row
/// commits of a logged table (each one waits for its WAL flush), printed as
/// p50 and maximum of 50. Admissions on one scope serialize on its rows, so
/// a burst's slowest wait grows with this.
async fn commit_latency(owner: &PgPool) {
    sqlx::query("CREATE TABLE omg_test_commit_latency(n integer)")
        .execute(owner)
        .await
        .unwrap();
    let mut latencies = Vec::new();
    for n in 0..50 {
        let start = Instant::now();
        sqlx::query("INSERT INTO omg_test_commit_latency VALUES ($1)")
            .bind(n)
            .execute(owner)
            .await
            .unwrap();
        latencies.push(start.elapsed());
    }
    sqlx::query("DROP TABLE omg_test_commit_latency")
        .execute(owner)
        .await
        .unwrap();
    eprintln!(
        "lock_stall commit_latency commits=50 p50_us={} max_us={}",
        quantile(&mut latencies, 0.5).as_micros(),
        quantile(&mut latencies, 1.0).as_micros()
    );
}

/// The commit delay (the cluster's slow commits).
const COMMIT_MS: u32 = 5;
/// Offered requests per second: one replica serves about 40 per second on
/// one scope with these commits on a developer machine, flat (and sheds the
/// rest with 429 at 60), so this leaves a wide margin on a slower runner.
const RATE: f64 = 10.0;
/// The upstream's answer time: requests are between admission and
/// settlement while it runs, so a stall catches settlements in flight.
const UPSTREAM_MS: u64 = 250;
const BEFORE: Duration = Duration::from_secs(5);
/// Longer than the 3 s settlement deadline, as on the cluster.
const STALL: Duration = Duration::from_secs(5);
const AFTER: Duration = Duration::from_secs(20);
const WINDOW: Duration = Duration::from_millis(500);
/// How soon after the stall ends the load must be back to its baseline.
const RECOVERY: Duration = Duration::from_secs(10);

#[dbtest::test]
#[ignore = "dedicated PostgreSQL runtime-role lock-stall measurement (#132)"]
async fn steady_load_recovers_after_a_stall(owner: PgPool) {
    let (setup, _role_lock) = setup(&owner).await;
    on_multi_thread(stall_recovery(setup)).await;
}
/// The recovery scenario's parameters: the CI configuration by default, or
/// a measurement's from `OMG_LOCK_STALL_*` (for a before/after comparison
/// of a change; never set in CI).
struct Knobs {
    replicas: usize,
    commit_ms: u32,
    upstream_ms: u64,
    stall: Duration,
    /// Offered requests per second.
    rate: f64,
    /// Print the numbers without asserting the shed and recovery bounds (a
    /// before/after comparison); the accounting is still asserted.
    measure: bool,
}
impl Knobs {
    fn from_env() -> Self {
        fn var<T: std::str::FromStr>(name: &str) -> Option<T> {
            std::env::var(format!("OMG_LOCK_STALL_{name}"))
                .ok()
                .map(|v| {
                    v.parse()
                        .unwrap_or_else(|_| panic!("OMG_LOCK_STALL_{name} is not a number"))
                })
        }
        let knobs = Self {
            replicas: var("REPLICAS").unwrap_or(1),
            commit_ms: var("COMMIT_MS").unwrap_or(COMMIT_MS),
            upstream_ms: var("UPSTREAM_MS").unwrap_or(UPSTREAM_MS),
            stall: var("STALL_MS").map_or(STALL, Duration::from_millis),
            rate: var("RATE").unwrap_or(RATE),
            measure: var::<u8>("MEASURE").is_some_and(|m| m == 1),
        };
        assert!(knobs.replicas >= 1, "OMG_LOCK_STALL_REPLICAS is at least 1");
        assert!(
            knobs.rate.is_finite() && knobs.rate >= 1.0,
            "OMG_LOCK_STALL_RATE is at least 1 request per second"
        );
        assert!(
            !knobs.stall.is_zero(),
            "OMG_LOCK_STALL_STALL_MS is positive"
        );
        knobs
    }
}

async fn stall_recovery(setup: Setup) {
    let knobs = Knobs::from_env();
    let h = harness(
        setup,
        knobs.upstream_ms,
        knobs.replicas,
        ADMISSION_LOCK_WAIT,
    )
    .await;
    let (ws, key) = prepare(&h, &knobs).await;
    let run = offer(&h, ws, &key, knobs.rate, knobs.stall).await;
    let rows = windows(&run);
    summarize(&h, &run).await;
    assess(&rows, &run, &knobs);
    let (settled, unknown) = assert_exact(&h, &[]).await;
    let in_flight = in_flight_at_stall(&run);
    eprintln!(
        "lock_stall recovery settled={settled} unknown={unknown} in_flight_at_stall={in_flight} offered={}",
        run.sent.len()
    );
    // Only settlements caught in flight by the stall end unknown: the
    // requests sent into it were shed, and the ones after it settle.
    assert!(
        knobs.measure || unknown as usize <= in_flight,
        "only the settlements in flight at the stall's start end unknown"
    );
}

/// The slow commits and a warm scope; asserts the delay fires twice per
/// request (admission and settlement). Returns (workspace, key).
async fn prepare(h: &Harness, knobs: &Knobs) -> (Uuid, String) {
    slow_commits(&h.owner, knobs.commit_ms).await;
    let (ws, key) = new_scope(h).await;
    burst(h, &key, 1).await;
    let delayed = async || -> i64 {
        sqlx::query_scalar("SELECT last_value FROM omg_test_slow_commits")
            .fetch_one(&h.owner)
            .await
            .unwrap()
    };
    let before = delayed().await;
    let sequential = Instant::now();
    for _ in 0..10 {
        burst(h, &key, 1).await;
    }
    let sequential = sequential.elapsed() / 10;
    let delayed = (delayed().await - before) as f64 / 10.0;
    eprintln!(
        "lock_stall recovery replicas={} commit_ms={} upstream_ms={} delayed_commits_per_request={delayed} sequential_ms_per_request={} offered_per_s={:.1}",
        knobs.replicas,
        knobs.commit_ms,
        knobs.upstream_ms,
        ms(sequential),
        knobs.rate
    );
    assert!(
        (1.5..=2.5).contains(&delayed),
        "the slow commit fires at every admission and settlement"
    );
    (ws, key)
}

struct Run {
    sent: Vec<Sent>,
    total: Duration,
    /// When the stall began and ended, since the start.
    stalled: Duration,
    released: Duration,
    /// Sessions the database opened during the run (connection churn).
    sessions_opened: i64,
}

/// Database sessions opened so far.
async fn sessions(owner: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT sessions FROM pg_stat_database WHERE datname=current_database()")
        .fetch_one(owner)
        .await
        .unwrap()
}

/// Open-loop load at `rate` for `BEFORE + stall + AFTER`, round robin over
/// the replicas. The stall, on a task of its own so the load keeps its
/// pace: a dedicated owner session holds the scope's lifetime totals row,
/// which every admission and settlement of the scope locks, from `BEFORE`
/// for `stall`.
async fn offer(h: &Harness, ws: Uuid, key: &str, rate: f64, stall: Duration) -> Run {
    let blocker = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            (*h.owner.connect_options())
                .clone()
                .application_name("omg_lock_stall_blocker")
                .disable_statement_logging(),
        )
        .await
        .unwrap();
    let opened = sessions(&h.owner).await;
    let start = Instant::now();
    let stall_task = tokio::spawn(async move {
        tokio::time::sleep(BEFORE).await;
        let mut tx = crate::db::begin(&blocker).await.unwrap();
        let locked = sqlx::query("SELECT 1 FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime' FOR UPDATE")
            .bind(ws).fetch_all(&mut *tx).await.unwrap().len();
        assert_eq!(locked, 1, "the stall holds the scope's lifetime totals row");
        let stalled = start.elapsed();
        tokio::time::sleep(stall).await;
        tx.rollback().await.unwrap();
        let released = start.elapsed();
        blocker.close().await;
        (stalled, released)
    });
    let total = BEFORE + stall + AFTER;
    let n = (total.as_secs_f64() * rate) as usize;
    let mut sends = Vec::with_capacity(n);
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / rate));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    for _ in 0..n {
        interval.tick().await;
        let url = h.replicas[sends.len() % h.replicas.len()].2.clone();
        sends.push(tokio::spawn(send(
            h.client.clone(),
            url,
            key.to_owned(),
            start,
        )));
    }
    let (stalled, released) = stall_task.await.unwrap();
    let mut sent = Vec::with_capacity(n);
    for s in sends {
        sent.push(tokio::time::timeout(GUARD, s).await.unwrap().unwrap());
    }
    Run {
        sent,
        total,
        stalled,
        released,
        sessions_opened: sessions(&h.owner).await - opened,
    }
}

/// Requests in flight when the stall began: sent before it, answered after.
fn in_flight_at_stall(run: &Run) -> usize {
    run.sent
        .iter()
        .filter(|s| s.at < run.stalled && s.at + s.latency > run.stalled)
        .count()
}

/// One window of send time: (start, offered, succeeded, p99 of the ones
/// that succeeded).
type Row = (Duration, usize, usize, Duration);

/// The counts and the p99 of the ones that succeeded among `sent`:
/// (sent, ok, shed, refused for accounting, p99 ms).
fn counts(sent: &[&Sent]) -> (usize, usize, usize, usize, u128) {
    let ok = sent.iter().filter(|s| s.status == 200).count();
    let shed = sent.iter().filter(|s| s.shed).count();
    let accounting = sent.iter().filter(|s| s.accounting_unavailable).count();
    let mut l: Vec<Duration> = sent
        .iter()
        .filter(|s| s.status == 200)
        .map(|s| s.latency)
        .collect();
    (sent.len(), ok, shed, accounting, ms(quantile(&mut l, 0.99)))
}

/// Per window: offered, succeeded, shed, refused for accounting, other
/// failures by status, and the latency quantiles of the ones that succeeded.
fn windows(run: &Run) -> Vec<Row> {
    let count = run.total.as_millis().div_ceil(WINDOW.as_millis()) as usize;
    let mut rows = Vec::new();
    for w in 0..count {
        let lo = WINDOW * w as u32;
        let sent: Vec<&Sent> = run
            .sent
            .iter()
            .filter(|s| s.at >= lo && s.at < lo + WINDOW)
            .collect();
        let (offered, ok, shed, accounting, _) = counts(&sent);
        let mut latencies: Vec<Duration> = sent
            .iter()
            .filter(|s| s.status == 200)
            .map(|s| s.latency)
            .collect();
        let (p50, p99) = (
            quantile(&mut latencies, 0.5),
            quantile(&mut latencies, 0.99),
        );
        let mut statuses = std::collections::BTreeMap::<u16, usize>::new();
        for s in sent
            .iter()
            .filter(|s| s.status != 200 && !s.accounting_unavailable && !s.shed)
        {
            *statuses.entry(s.status).or_default() += 1;
        }
        eprintln!(
            "lock_stall recovery window_ms={} offered={offered} ok={ok} shed={shed} accounting_unavailable={accounting} other_failures={statuses:?} p50_ms={} p99_ms={}",
            ms(lo),
            ms(p50),
            ms(p99)
        );
        rows.push((lo, offered, ok, p99));
    }
    rows
}

/// From the first second until 2 s before the stall, during it, after its
/// release, and in the last 5 s: (sent, ok, shed, accounting refusals, p99
/// ms).
async fn summarize(h: &Harness, run: &Run) {
    let phase = |lo: Duration, hi: Duration| {
        let p: Vec<&Sent> = run
            .sent
            .iter()
            .filter(|s| s.at >= lo && s.at < hi)
            .collect();
        counts(&p)
    };
    let pre = phase(
        Duration::from_secs(1),
        run.stalled.saturating_sub(Duration::from_secs(2)),
    );
    let during = phase(run.stalled, run.released);
    let after = phase(run.released, run.total);
    let late = phase(run.total.saturating_sub(Duration::from_secs(5)), run.total);
    eprintln!(
        "lock_stall recovery summary replicas={} pre_stall={pre:?} stall={during:?} after_release={after:?} last_5s={late:?} (sent,ok,shed,accounting_unavailable,p99_ms) in_flight_at_stall={} sessions_opened_during_run={}",
        h.replicas.len(),
        in_flight_at_stall(run),
        run.sessions_opened
    );
}

/// The requests sent into the stall were shed before admission; the
/// baseline is within capacity; and from some point within `RECOVERY` of
/// the release to the end of the run, at most one window falls short (fewer
/// than 95% succeeded, or a p99 above the bound). Windows with no request
/// are skipped.
fn assess(rows: &[Row], run: &Run, knobs: &Knobs) {
    let stall = run.released - run.stalled;
    // Requests sent into the stall (and not in its last bound, which the
    // release may cut short) are shed when the bound elapses, before any
    // reservation or upstream call, with an honest `Retry-After`.
    let into: Vec<&Sent> = run
        .sent
        .iter()
        .filter(|s| {
            s.at >= run.stalled
                && s.at + ADMISSION_LOCK_WAIT + Duration::from_secs(1) < run.released
        })
        .collect();
    let mut latencies: Vec<Duration> = into.iter().map(|s| s.latency).collect();
    let (median, slowest) = (quantile(&mut latencies, 0.5), quantile(&mut latencies, 1.0));
    eprintln!(
        "lock_stall recovery stall_ms={} released_at_ms={} sent_into_stall={} shed={} median_ms={} slowest_ms={}",
        ms(stall),
        ms(run.released),
        into.len(),
        into.iter().filter(|s| s.shed).count(),
        ms(median),
        ms(slowest)
    );
    let check = |ok: bool, what: &str| assert!(knobs.measure || ok, "{what}");
    check(!into.is_empty(), "requests were sent into the stall");
    check(
        into.iter().all(|s| {
            s.shed && s.retry_after == Some(crate::inference::error::SCOPE_BUSY_RETRY_SECONDS)
        }),
        "every request sent into the stall is shed with Retry-After, not left waiting",
    );
    // Shed when the bound elapses (the time outside admission, a busy pool
    // or a slow disk, adds to some), never after the stall's length.
    check(
        median <= ADMISSION_LOCK_WAIT + Duration::from_millis(500)
            && slowest + Duration::from_secs(1) < stall,
        "shed when the bound elapses, not after the stall",
    );
    // The windows before the stall after the first second, without the
    // requests still in flight when it began.
    let in_flight = WINDOW + Duration::from_millis(knobs.upstream_ms) + Duration::from_secs(1);
    let baseline: Vec<&Row> = rows
        .iter()
        .filter(|r| r.1 > 0 && r.0 >= Duration::from_secs(1) && r.0 + in_flight <= run.stalled)
        .collect();
    let (sent, ok) = baseline.iter().fold((0, 0), |(s, o), r| (s + r.1, o + r.2));
    let base_ok = ok as f64 / sent.max(1) as f64;
    let mut base_p99: Vec<Duration> = baseline.iter().map(|r| r.3).collect();
    let base_p99 = quantile(&mut base_p99, 0.5);
    eprintln!(
        "lock_stall recovery baseline_ok_fraction={base_ok:.2} baseline_median_p99_ms={}",
        ms(base_p99)
    );
    check(
        sent > 0 && base_ok >= 0.99,
        "the load is within capacity before the stall",
    );
    let p99_bound = base_p99 * 3 + Duration::from_millis(100);
    let after: Vec<&Row> = rows
        .iter()
        .filter(|r| r.1 > 0 && r.0 >= run.released)
        .collect();
    let bad = |r: &Row| (r.2 as f64) < 0.95 * r.1 as f64 || r.3 > p99_bound;
    // The earliest good window from which at most one window falls short.
    let recovered = (0..after.len())
        .find(|&i| !bad(after[i]) && after[i..].iter().filter(|r| bad(r)).count() <= 1)
        .map(|i| after[i].0.saturating_sub(run.released));
    eprintln!(
        "lock_stall recovery recovered_after_ms={} bound_ms={} p99_bound_ms={}",
        recovered.map_or("never".into(), |r| ms(r).to_string()),
        ms(RECOVERY),
        ms(p99_bound)
    );
    check(
        recovered.is_some_and(|r| r <= RECOVERY),
        &format!(
            "throughput and latency did not return to the pre-stall level within {}ms of the release",
            ms(RECOVERY)
        ),
    );
}
