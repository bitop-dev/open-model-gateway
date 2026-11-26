//! The guardrail step's bounds end to end (#840, docs/guardrails-design.md
//! §4 "Capacity"): real PostgreSQL and the HTTP router. No scan slot in
//! time is a retryable 503 on every protocol; a detector failure follows the
//! rule's `on_error`; a key that keeps being refused is throttled before the
//! scan; a request writes a bounded number of decision rows; and the cached
//! policy costs no statement on a hit and follows every change.
use std::sync::{Arc, atomic::Ordering};

use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::guardrail_db_tests::{World, events, secret, type_default, world};
use crate::auth::NewApiKey;
use crate::guardrails::detector::Inspection;
use crate::guardrails::limits::{self, REFUSALS, REFUSALS_PER_MINUTE, SHARE, SHARED_SCANS};
use crate::guardrails::policy::Workload;
use crate::store::Store;

async fn post(base: &str, token: &str, path: &str, body: &Value) -> (u16, HeaderMap, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    (
        status,
        headers,
        response.json().await.unwrap_or(Value::Null),
    )
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).map(|v| v.to_str().unwrap())
}
fn bodies(text: &str) -> [(&'static str, Value); 3] {
    [
        (
            "/v1/chat/completions",
            json!({"model":"company/smart","messages":[{"role":"user","content":text}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"company/smart","input":text,"max_output_tokens":16}),
        ),
        (
            "/v1/messages",
            json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":text}]}),
        ),
    ]
}
/// The error object of either envelope (both nest it under `error`).
fn error(body: &Value) -> &Value {
    &body["error"]
}

/// The type-default rule of `kind` (the team workspace's here).
async fn team_rule(w: &World, action: &str) {
    let pool = &w.f.store.pool;
    let set = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default','team',false,$2)")
        .bind(set).bind(w.f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Secrets','secrets','pattern','{secret}',$3,'user_text','off','fail_closed','{generation,embeddings}','deny')")
        .bind(Uuid::now_v7()).bind(set).bind(action).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}
async fn team_token(w: &World) -> String {
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'guardrail limits',$4)")
        .bind(key.id).bind(w.f.team.workspace_id).bind(w.f.team.user_id).bind(key.digest.as_slice())
        .execute(&w.f.store.pool).await.unwrap();
    key.token
}

/// While one workspace holds its whole share of the scan slots, its next
/// scan waits the bounded time and is refused with a retryable 503 on every
/// protocol; another workspace's request is not delayed or refused.
#[dbtest::test]
async fn no_scan_slot_is_a_retryable_503_and_other_workspaces_are_served(pool: PgPool) {
    let w = world(pool, "'chat_completions','messages','responses'").await;
    let (set, rule) = type_default(&w, "block", "{custom}", &["ACME-[0-9]{4}".into()]).await;
    team_rule(&w, "block").await;
    let team = team_token(&w).await;
    let flood = w.f.principal.workspace_id;
    let mut held = Vec::new();
    for _ in 0..SHARE {
        held.push(SHARED_SCANS.acquire(flood).await.unwrap());
    }
    let expected = json!({"policy_id": set, "rule_id": rule, "stage": "input", "category": "custom", "detector": "pattern"});
    let clean = bodies("hello")[0].1.clone();
    for (path, body) in bodies("nothing to see") {
        let (status, headers, body) = post(&w.base, &w.token, path, &body).await;
        assert_eq!(status, 503, "{path} {body}");
        assert_eq!(header(&headers, "retry-after"), Some("1"), "{path}");
        assert_ne!(header(&headers, "x-should-retry"), Some("false"), "{path}");
        assert_eq!(error(&body)["guardrail"], expected, "{path}");
        if path == "/v1/messages" {
            assert_eq!(body["type"], "error");
            assert_eq!(error(&body)["type"], "overloaded_error");
        } else {
            assert_eq!(error(&body)["code"], "guardrail_unavailable", "{path}");
            assert_eq!(error(&body)["type"], "server_error", "{path}");
        }
        // The team workspace is served meanwhile (its built-in-only policy
        // never waits for a slot).
        let (status, _, body) = post(&w.base, &team, "/v1/chat/completions", &clean).await;
        assert_eq!(status, 200, "{body}");
    }
    drop(held);
    assert_eq!(
        w.hits.load(Ordering::SeqCst),
        3,
        "only the team's requests reached the provider"
    );
    // A busy refusal scanned nothing: it writes no row and does not count
    // toward the key's refusal bound.
    assert!(events(&w).await.is_empty());
    assert_eq!(REFUSALS.counted(w.f.principal.key_id, limits::now()), 0);
    // With the slots free again the same request is inspected and passes.
    let (status, _, _) = post(
        &w.base,
        &w.token,
        "/v1/chat/completions",
        &bodies("ok")[0].1,
    )
    .await;
    assert_eq!(status, 200);
}

/// A rule whose pattern this release cannot compile is a detector failure:
/// fail-closed refuses with the non-retryable 503 on every protocol,
/// fail-open lets the request through and records it.
#[dbtest::test]
async fn a_detector_failure_follows_on_error_on_every_protocol(pool: PgPool) {
    let w = world(pool, "'chat_completions','messages','responses'").await;
    let (set, rule) = type_default(&w, "block", "{custom}", &["(unclosed".into()]).await;
    let expected = json!({"policy_id": set, "rule_id": rule, "stage": "input", "category": "custom", "detector": "pattern"});
    for (path, body) in bodies("hello") {
        let (status, headers, body) = post(&w.base, &w.token, path, &body).await;
        assert_eq!(status, 503, "{path} {body}");
        assert_eq!(header(&headers, "x-should-retry"), Some("false"), "{path}");
        assert_eq!(header(&headers, "retry-after"), None, "{path}");
        assert_eq!(error(&body)["guardrail"], expected, "{path}");
        if path == "/v1/messages" {
            assert_eq!(error(&body)["type"], "api_error");
        } else {
            assert_eq!(error(&body)["code"], "guardrail_detector_failed", "{path}");
        }
    }
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
    // The same pattern under a fail-open monitor rule.
    type_default(&w, "monitor", "{custom}", &["(unclosed".into()]).await;
    let (status, _, _) = post(
        &w.base,
        &w.token,
        "/v1/chat/completions",
        &bodies("hello")[0].1,
    )
    .await;
    assert_eq!(status, 200);
    let last = events(&w).await.pop().unwrap();
    assert_eq!(
        (last.1.as_str(), last.2.as_str()),
        ("detector_failed", "custom")
    );
}

/// A key refused `REFUSALS_PER_MINUTE` times this minute gets 429 before any
/// scan, and nothing is recorded for the throttled request.
#[dbtest::test]
async fn a_key_that_keeps_being_refused_is_throttled_before_the_scan(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    type_default(&w, "block", "{secret}", &[]).await;
    let key = w.f.principal.key_id;
    let leak = bodies(&format!("use {}", secret()))[0].1.clone();
    // The bound counts per minute: a run that crosses a minute boundary
    // starts again (at most twice).
    for attempt in 0.. {
        let minute = limits::now() / 60;
        for _ in 1..REFUSALS_PER_MINUTE {
            REFUSALS.note(key, limits::now());
        }
        let recorded = events(&w).await.len();
        let (last, _, _) = post(&w.base, &w.token, "/v1/chat/completions", &leak).await;
        let (status, headers, body) = post(&w.base, &w.token, "/v1/chat/completions", &leak).await;
        if limits::now() / 60 != minute {
            assert!(attempt < 2, "the minute changed during every attempt");
            continue;
        }
        assert_eq!(last, 400, "the last refusal within the bound is inspected");
        assert_eq!(status, 429, "{body}");
        assert_eq!(error(&body)["code"], "guardrail_rate_limited");
        let retry: u32 = header(&headers, "retry-after").unwrap().parse().unwrap();
        assert!((1..=60).contains(&retry));
        assert_eq!(
            events(&w).await.len(),
            recorded + 1,
            "the throttled request wrote no row"
        );
        break;
    }
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
}

/// A request writes at most `MAX_EVENT_ROWS` decision rows: the refused one
/// first, the rest counted on it.
#[dbtest::test]
async fn decisions_beyond_the_row_cap_are_counted_on_the_refused_row(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    let (set, _) = type_default(&w, "monitor", "{custom}", &["TAG-0".into()]).await;
    let mut tx = w.f.store.pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1 WHERE id=$1")
        .bind(set)
        .execute(&mut *tx)
        .await
        .unwrap();
    let block = Uuid::now_v7();
    for position in 1..20_i16 {
        let action = if position == 19 { "block" } else { "monitor" };
        let on_error = if position == 19 {
            "fail_closed"
        } else {
            "fail_open"
        };
        let id = if position == 19 {
            block
        } else {
            Uuid::now_v7()
        };
        sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,$3,'Rule','custom','pattern','{custom}',$4,'user_text','off',$5,'{generation}','deny',ARRAY[$6])")
            .bind(id).bind(set).bind(position).bind(action).bind(on_error).bind(format!("TAG-{position}"))
            .execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let text: Vec<String> = (0..20).map(|i| format!("TAG-{i}")).collect();
    let (status, _, _) = post(
        &w.base,
        &w.token,
        "/v1/chat/completions",
        &bodies(&text.join(" "))[0].1,
    )
    .await;
    assert_eq!(status, 400);
    let rows: Vec<(Uuid, bool, i32)> =
        sqlx::query_as("SELECT rule_id,refused,omitted FROM guardrail_events ORDER BY id")
            .fetch_all(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), limits::MAX_EVENT_ROWS);
    assert_eq!(
        rows[0],
        (block, true, 4),
        "the refused row first, with the 4 left out"
    );
    assert!(rows[1..].iter().all(|r| !r.1 && r.2 == 0));
}

/// A store whose configuration versions are confirmed now and never learn
/// about changes until `poll_once`.
async fn replica(pool: &PgPool, confirmed: bool) -> Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let store = Store::new(pool);
    if confirmed {
        confirm(&store).await;
    }
    store
}
async fn confirm(store: &Store) {
    store.caches.versions.enable();
    crate::notify::poll_once(&store.pool, &store.caches.versions)
        .await
        .unwrap();
    store.caches.versions.pin_sync();
}
fn refuses(policy: &crate::guardrails::enforce::Policy) -> bool {
    let k = secret();
    let mut input = Inspection::default();
    input.push(&k, crate::guardrails::detector::Part::Content);
    let refusal = policy.decide(Workload::Generation, Some(&input)).refusal;
    if let Some(r) = refusal {
        assert_eq!(
            (r.kind, r.category),
            (
                crate::guardrails::enforce::RefusalKind::InputBlocked,
                crate::guardrails::policy::Category::Secret
            )
        );
    }
    refusal.is_some()
}

#[dbtest::test]
async fn a_cached_policy_costs_no_statement_and_follows_every_change(pool: PgPool) {
    let w = world(pool.clone(), "'chat_completions'").await;
    let ws = w.f.principal.workspace_id;
    type_default(&w, "monitor", "{secret}", &[]).await;
    // A hit costs no statement: it is served with the replica's pool closed.
    let closed = replica(&pool, true).await;
    let first = closed.guardrail_policy(ws).await.unwrap();
    closed.pool.close().await;
    let hit = closed.guardrail_policy(ws).await.unwrap();
    assert!(Arc::ptr_eq(&first, &hit));
    // A tightening takes effect once the replica learns of the bump; until
    // then the bounded staleness of every cache applies.
    let stale = replica(&pool, true).await;
    let before = stale.guardrail_policy(ws).await.unwrap();
    assert!(!refuses(&before));
    type_default(&w, "block", "{secret}", &[]).await;
    crate::notify::poll_once(&stale.pool, &stale.caches.versions)
        .await
        .unwrap();
    let after = stale.guardrail_policy(ws).await.unwrap();
    assert!(
        refuses(&after),
        "the stale entry was never used once the bump was seen"
    );
    // A stale entry whose rule sets did not change is revalidated, not
    // recompiled: a change of an unrelated row bumps the topic only.
    sqlx::query("UPDATE guardrail_rule_sets SET updated_by=updated_by WHERE false")
        .execute(&pool)
        .await
        .unwrap();
    stale.caches.versions.flush();
    let revalidated = stale.guardrail_policy(ws).await.unwrap();
    assert!(Arc::ptr_eq(&after, &revalidated));
    // Unconfirmed versions (the listener is down): every lookup reads the
    // heads live, reuses the compiled policy while they are equal, and
    // follows a change at once.
    let bypass = replica(&pool, false).await;
    let a = bypass.guardrail_policy(ws).await.unwrap();
    let b = bypass.guardrail_policy(ws).await.unwrap();
    assert!(Arc::ptr_eq(&a, &b), "no recompile per request");
    type_default(&w, "monitor", "{secret}", &[]).await;
    let c = bypass.guardrail_policy(ws).await.unwrap();
    assert!(!refuses(&c), "a loosening applies at once too");
    // Concurrent misses coalesce into one compile.
    let cold = Arc::new(replica(&pool, true).await);
    let loads = futures_util::future::join_all((0..16).map(|_| {
        let cold = Arc::clone(&cold);
        tokio::spawn(async move { cold.guardrail_policy(ws).await.unwrap() })
    }))
    .await;
    let loads: Vec<_> = loads.into_iter().map(Result::unwrap).collect();
    assert!(loads.iter().all(|p| Arc::ptr_eq(p, &loads[0])));
}

/// Requests that wrote decision rows are bounded per key too: the step runs
/// before admission, so a key whose requests admission refuses could
/// otherwise make the replica scan and write rows without bound.
#[dbtest::test]
async fn a_key_whose_requests_keep_writing_rows_is_throttled_before_the_scan(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    type_default(&w, "monitor", "{secret}", &[]).await;
    let key = w.f.principal.key_id;
    let monitored = bodies(&format!("use {}", secret()))[0].1.clone();
    for attempt in 0.. {
        let minute = limits::now() / 60;
        for _ in 1..limits::RECORDED_PER_MINUTE {
            limits::RECORDED.note(key, limits::now());
        }
        let (last, _, _) = post(&w.base, &w.token, "/v1/chat/completions", &monitored).await;
        let recorded = events(&w).await.len();
        let (status, headers, body) =
            post(&w.base, &w.token, "/v1/chat/completions", &monitored).await;
        if limits::now() / 60 != minute {
            assert!(attempt < 2, "the minute changed during every attempt");
            continue;
        }
        assert_eq!(last, 200, "the last recorded request within the bound runs");
        assert_eq!(status, 429, "{body}");
        assert_eq!(error(&body)["code"], "guardrail_rate_limited");
        assert!(header(&headers, "retry-after").is_some());
        assert_eq!(
            events(&w).await.len(),
            recorded,
            "the throttled request wrote no row"
        );
        break;
    }
}
