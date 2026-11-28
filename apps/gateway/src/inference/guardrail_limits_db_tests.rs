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

use super::guardrail_db_tests::{World, events, secret, type_default, type_default_with, world};
use crate::auth::NewApiKey;
use crate::guardrails::detector::Inspection;
use crate::guardrails::limits::{
    self, LOCAL_SCANS, RECORDED, RECORDED_PER_MINUTE, SHARE, SHARED_SCANS,
};
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
/// A new key of the principal's workspace: a run of the per-minute bound
/// that crossed a minute boundary starts again with a key counted nowhere.
async fn fresh_key(w: &World) -> (String, Uuid) {
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'guardrail limits',$4)")
        .bind(key.id).bind(w.f.principal.workspace_id).bind(w.f.owner).bind(key.digest.as_slice())
        .execute(&w.f.store.pool).await.unwrap();
    (key.token, key.id)
}
async fn team_token(w: &World) -> String {
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'guardrail limits',$4)")
        .bind(key.id).bind(w.f.team.workspace_id).bind(w.f.team.user_id).bind(key.digest.as_slice())
        .execute(&w.f.store.pool).await.unwrap();
    key.token
}

/// A personal-data scan of more than a few KiB runs in a scan slot (a
/// local layer's rule in the local pool), one of less runs inline, and a
/// scan whose budget ran out is the same retryable 503 as a busy gateway's.
#[dbtest::test]
async fn a_personal_data_scan_takes_a_slot_above_a_few_kib_and_its_budget_is_enforced(
    pool: PgPool,
) {
    let w = world(pool, "'chat_completions'").await;
    let (_, rule) = type_default_with(&w, "block", "{personal_data}", &[], &["card"]).await;
    let ws = w.f.principal.workspace_id;
    let chat = |content: String| json!({"model":"company/smart","messages":[{"role":"user","content":content}]});
    let (small, large) = (chat("hello".into()), chat("word ".repeat(2_000)));
    let busy = |status: u16, body: &Value| {
        assert_eq!(status, 503, "{body}");
        assert_eq!(error(body)["code"], "guardrail_unavailable", "{body}");
        assert_eq!(
            error(body)["guardrail"]["category"],
            "personal_data",
            "{body}"
        );
    };
    let mut held = Vec::new();
    for _ in 0..SHARE {
        held.push(SHARED_SCANS.acquire(ws).await.unwrap());
    }
    let (status, _, body) = post(&w.base, &w.token, "/v1/chat/completions", &small).await;
    assert_eq!(status, 200, "inline below a few KiB: {body}");
    let (status, headers, body) = post(&w.base, &w.token, "/v1/chat/completions", &large).await;
    busy(status, &body);
    assert_eq!(header(&headers, "retry-after"), Some("1"));
    drop(held);
    // A personal-data rule of a Team's local layer (written by one of its
    // owners or admins): its scans use the local pool, so the Team's whole
    // share of the shared pool held does not stop them.
    let team = w.f.team.workspace_id;
    let token = team_token(&w).await;
    let local = Uuid::now_v7();
    let mut tx = w.f.store.pool.begin().await.unwrap();
    let writer: Uuid = sqlx::query_scalar("SELECT user_id FROM effective_workspace_memberships WHERE workspace_id=$1 AND role IN ('owner','admin') LIMIT 1")
        .bind(team).fetch_one(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,workspace_id,updated_by) VALUES($1,'workspace_local',$2,$3)")
        .bind(local).bind(team).bind(writer).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,kinds) VALUES($1,$2,0,'Mail','personal_data','pattern','{personal_data}','monitor','user_text','off','fail_open','{generation}','deny','{email}')")
        .bind(Uuid::now_v7()).bind(local).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut held = Vec::new();
    for _ in 0..SHARE {
        held.push(SHARED_SCANS.acquire(team).await.unwrap());
    }
    let (status, _, body) = post(&w.base, &token, "/v1/chat/completions", &large).await;
    assert_eq!(status, 200, "the local pool: {body}");
    let mut local_held = Vec::new();
    for _ in 0..SHARE {
        local_held.push(LOCAL_SCANS.acquire(team).await.unwrap());
    }
    drop(held);
    let (status, _, body) = post(&w.base, &token, "/v1/chat/completions", &large).await;
    busy(status, &body);
    drop(local_held);
    // Out of budget: refused, retryable, and nothing is sent.
    let hits = w.hits.load(Ordering::SeqCst);
    crate::guardrails::enforce::SPEND_BUDGET
        .lock()
        .unwrap()
        .insert(rule, [true].into());
    let (status, headers, body) = post(&w.base, &w.token, "/v1/chat/completions", &small).await;
    busy(status, &body);
    assert_ne!(header(&headers, "x-should-retry"), Some("false"));
    assert_eq!(w.hits.load(Ordering::SeqCst), hits, "nothing was sent");
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
    // toward the key's bound.
    assert!(events(&w).await.is_empty());
    assert_eq!(RECORDED.counted(w.f.principal.key_id, limits::now()), 0);
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

/// A refusal wrote rows and was never admitted, so it counts toward the
/// key's one bound: at `RECORDED_PER_MINUTE` this minute the key gets 429
/// before any scan, nothing is recorded for the throttled request, and the
/// unscanned counter says why.
#[dbtest::test]
async fn refusals_count_toward_the_one_bound_and_a_throttled_request_is_counted(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    type_default(&w, "block", "{secret}", &[]).await;
    let leak = bodies(&format!("use {}", secret()))[0].1.clone();
    // The bound counts per minute: a run that crosses a minute boundary
    // starts again with a new key (at most twice).
    for attempt in 0.. {
        let (token, key) = fresh_key(&w).await;
        let minute = limits::now() / 60;
        for _ in 1..RECORDED_PER_MINUTE {
            RECORDED.note(key, limits::now());
        }
        let recorded = events(&w).await.len();
        let (last, _, _) = post(&w.base, &token, "/v1/chat/completions", &leak).await;
        let (status, headers, body) = post(&w.base, &token, "/v1/chat/completions", &leak).await;
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
    let metrics = crate::metrics::METRICS.render(None).await;
    assert!(
        metrics.contains("gateway_guardrail_unscanned_total{reason=\"rate_limited\"}"),
        "the throttled request is counted"
    );
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

/// Requests that wrote decision rows and were never admitted (refused by
/// admission, failed or abandoned) are bounded per key too: the step runs
/// before admission, so a key whose requests admission refuses could
/// otherwise make the replica scan and write rows without bound. Admitted
/// monitored requests never count.
#[dbtest::test]
async fn a_key_whose_unadmitted_requests_keep_writing_rows_is_throttled(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    type_default(&w, "monitor", "{secret}", &[]).await;
    let monitored = bodies(&format!("use {}", secret()))[0].1.clone();
    for attempt in 0.. {
        let (token, key) = fresh_key(&w).await;
        let minute = limits::now() / 60;
        let before = RECORDED.counted(key, limits::now());
        let (answered, _, _) = post(&w.base, &token, "/v1/chat/completions", &monitored).await;
        let after_answered = RECORDED.counted(key, limits::now());
        // One request a minute: the next one is refused by admission.
        sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,requests_per_minute) VALUES($1,$2,1) ON CONFLICT DO NOTHING")
            .bind(w.f.principal.workspace_id)
            .bind(key)
            .execute(&w.f.store.pool)
            .await
            .unwrap();
        let (refused, _, _) = post(&w.base, &token, "/v1/chat/completions", &monitored).await;
        let after_refused = RECORDED.counted(key, limits::now());
        for _ in after_refused..RECORDED_PER_MINUTE {
            RECORDED.note(key, limits::now());
        }
        let recorded = events(&w).await.len();
        let (status, headers, body) =
            post(&w.base, &token, "/v1/chat/completions", &monitored).await;
        if limits::now() / 60 != minute {
            assert!(attempt < 2, "the minute changed during every attempt");
            continue;
        }
        assert_eq!(
            (answered, after_answered),
            (200, before),
            "an admitted request never counts"
        );
        assert_eq!(refused, 429, "admission refused the second request");
        assert_eq!(
            after_refused,
            before + 1,
            "the unadmitted request that wrote rows counts"
        );
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

/// Once an attempt is admitted, admission's own limits bound the request: a
/// monitored request whose provider then fails does not count toward the
/// key's bound of unadmitted recorded requests.
#[dbtest::test]
async fn an_admitted_request_that_fails_upstream_does_not_count(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    type_default(&w, "monitor", "{secret}", &[]).await;
    let key = w.f.principal.key_id;
    let monitored = bodies(&format!("use {}", secret()))[0].1.clone();
    // The provider is gone before the first request (no pooled connection
    // to it survives).
    drop(w.upstream);
    for attempt in 0.. {
        let minute = limits::now() / 60;
        let before = RECORDED.counted(key, limits::now());
        let (status, _, body) = post(&w.base, &w.token, "/v1/chat/completions", &monitored).await;
        let after = RECORDED.counted(key, limits::now());
        if limits::now() / 60 != minute {
            assert!(attempt < 2, "the minute changed during every attempt");
            continue;
        }
        assert!(status >= 500, "the provider failed: {status} {body}");
        let admitted: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
        assert!(admitted >= 1, "the attempt was admitted");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_events")
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
        assert!(rows >= 1, "its decisions wrote rows");
        assert_eq!(after, before, "an admitted request never counts");
        break;
    }
}

/// The policy is read in one snapshot: an override removed between the read
/// of the heads and the read of its rules is still read whole (never as an
/// empty set that would shadow the type default), and the next lookup reads
/// the type default.
#[dbtest::test]
async fn an_override_removed_between_the_reads_is_never_an_empty_policy(pool: PgPool) {
    let w = world(pool.clone(), "'chat_completions'").await;
    let ws = w.f.principal.workspace_id;
    type_default(&w, "block", "{secret}", &[]).await;
    let set = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,workspace_id,local_only,updated_by) VALUES($1,'workspace_override',$2,false,$3)")
        .bind(set).bind(ws).bind(w.f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Watch','secrets','pattern','{secret}','monitor','user_text','off','fail_open','{generation,embeddings}','deny')")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let store = Arc::new(replica(&pool, false).await);
    let (reached, resume) = crate::guardrails::stored::tests_pause::arm(ws);
    let load = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.guardrail_policy(ws).await.unwrap() })
    };
    reached.notified().await;
    // An admin resets the override while the snapshot is between its reads.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(w.f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
        .bind(set)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM guardrail_rule_sets WHERE id=$1")
        .bind(set)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    resume.notify_one();
    let during = load.await.unwrap();
    assert!(!during.is_empty(), "the override was read with its rule");
    assert!(
        !refuses(&during),
        "the override's monitor rule, as of the snapshot"
    );
    let after = store.guardrail_policy(ws).await.unwrap();
    assert!(refuses(&after), "then the type default");
}

/// One platform edit compiles its patterns once per replica: 200 workspaces
/// of one kind load their policies at once, before and after a change of the
/// type default, and none answers `guardrail_unavailable`.
#[dbtest::test]
async fn a_platform_layer_is_compiled_once_for_every_workspace_of_its_kind(pool: PgPool) {
    let w = world(pool.clone(), "'chat_completions'").await;
    let set = Uuid::now_v7();
    let rule = "INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,0,'Ids','custom','pattern','{custom}','block','user_text','off','fail_closed','{generation}','deny',ARRAY[$3])";
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default','project',false,$2)")
        .bind(set).bind(w.f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query(rule)
        .bind(Uuid::now_v7())
        .bind(set)
        .bind("ACME-[0-9]{4}")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let workspaces: Vec<Uuid> = (0..200).map(|_| Uuid::now_v7()).collect();
    sqlx::query("INSERT INTO workspaces(id,name,kind) SELECT id,'P-'||id,'project' FROM unnest($1::uuid[]) id")
        .bind(&workspaces)
        .execute(&pool)
        .await
        .unwrap();
    let store = Arc::new(replica(&pool, true).await);
    let load_all = || {
        futures_util::future::join_all(workspaces.iter().map(|ws| {
            let store = Arc::clone(&store);
            let ws = *ws;
            tokio::spawn(async move { store.guardrail_policy(ws).await })
        }))
    };
    for loaded in load_all().await {
        assert!(!loaded.unwrap().unwrap().is_empty(), "none answers 503");
    }
    assert_eq!(crate::guardrails::shared::compiles(set), 1);
    // The platform changes its pattern: one more compile, for all of them.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(w.f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
        .bind(set)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(rule)
        .bind(Uuid::now_v7())
        .bind(set)
        .bind("ACME-[0-9]{6}")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    crate::notify::poll_once(&store.pool, &store.caches.versions)
        .await
        .unwrap();
    for loaded in load_all().await {
        assert!(!loaded.unwrap().unwrap().is_empty(), "none answers 503");
    }
    assert_eq!(crate::guardrails::shared::compiles(set), 2);
}
