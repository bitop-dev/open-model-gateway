//! #804: a Platform Admin resolves the unknown responses of a finished
//! realtime session from evidence (`realtime_resolution::resolve_response`), on a
//! disposable database. The refusals mirror request resolution
//! (`resolve_usage`): never zero without proof, never a second settlement,
//! no lowering of recorded usage, nothing while the session is open, live
//! Platform Admin authority, personal workspaces only for their owner.
use super::realtime_resolution::{Resolved, SessionRefusal, resolve_response, resolve_session};
use super::tests::db::{Fixture, amounts, fixture};
use super::*;
use crate::inference::{
    realtime::{RealtimeFinish, RealtimeUsage, ResponseBound, ResponseStatus},
    repository::AttemptTelemetry,
};
use serde_json::{Value, json};
use sqlx::PgPool;

fn line(meter: &str, amount: &str) -> Value {
    json!({"meter":meter,"microusd_per_batch":amount,"batch":1000000,"unit_label":"/M tokens","sku_label":"Line"})
}
fn na(meter: &str) -> Value {
    json!({"meter":meter,"not_applicable":true})
}
/// $4/$0.40/$16 text, $32/$0.40/$64 audio per M tokens; other meters NA.
fn lines() -> Value {
    json!([
        line("input_tokens", "4000000"),
        line("cache_read_tokens", "400000"),
        line("output_tokens", "16000000"),
        line("input_audio_tokens", "32000000"),
        line("cache_read_audio_tokens", "400000"),
        line("output_audio_tokens", "64000000"),
        na("cache_write_tokens"), na("cache_write_5m_tokens"), na("cache_write_1h_tokens"),
        na("output_images"), na("input_characters"), na("input_audio_seconds_ms"),
        na("output_audio_seconds_ms"), na("search_units"),
        {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
    ])
}
const FULL: ResponseBound = ResponseBound {
    text_input: None,
    audio_input: None,
    output: 100,
};
/// One full window (see `realtime_tests`): 44,800 µUSD.
const WINDOW: i64 = 44_800;
const U: RealtimeUsage = RealtimeUsage {
    input_text_tokens: 10,
    cached_text_tokens: 4,
    input_audio_tokens: 20,
    cached_audio_tokens: 5,
    output_text_tokens: 10,
    output_audio_tokens: 40,
};
/// U valued at the pinned price.
const RESPONSE: i64 = 3_228;

async fn realtime_model(f: &Fixture, priced: bool) -> Deployment {
    realtime_model_priced(f, priced.then(lines)).await
}
async fn realtime_model_priced(f: &Fixture, price: Option<Value>) -> Deployment {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['realtime'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    if let Some(lines) = price {
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,100,3,$3,'{}')")
            .bind(Uuid::new_v4()).bind(f.deployment).bind(lines).execute(&f.store.pool).await.unwrap();
    }
    use crate::inference::repository::InferenceRepository;
    f.store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0)
}
/// An admitted session of `principal` on `d`, still open.
async fn open_session(f: &Fixture, principal: crate::auth::Principal, d: &Deployment) -> Uuid {
    let start = ExecutionStart {
        streamed: true,
        principal,
        ..f.start()
    };
    let admission = WorkloadAdmission {
        kind: WorkloadKind::Realtime,
        output: OutputReservation::Requested(Some(100)),
        unit_ceilings: MeterUsage {
            requests: Some(1),
            ..MeterUsage::default()
        },
    };
    admit_workload_for_deployment(&f.store, &start, &admission, 960, d)
        .await
        .unwrap();
    start.id
}
/// Response `n` with a full window, settled from `usage` (unknown when None).
async fn respond(
    f: &Fixture,
    principal: &crate::auth::Principal,
    id: Uuid,
    n: i32,
    usage: Option<RealtimeUsage>,
) {
    realtime::reserve_window(&f.store, principal, id, "company/smart", FULL, None)
        .await
        .unwrap();
    realtime::open_response(&f.store, id, n, FULL)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        id,
        n,
        Some(ResponseStatus::Completed),
        usage,
        FULL,
    )
    .await
    .unwrap();
}
async fn end(f: &Fixture, id: Uuid) {
    realtime::finish(
        &f.store,
        &RealtimeFinish {
            id,
            outcome: Outcome::Succeeded,
            error: None,
            elapsed_ms: 5,
            telemetry: AttemptTelemetry::default(),
            unopened_request: false,
            window: FULL,
            recorded_sequence: 0,
        },
    )
    .await
    .unwrap();
}
/// A finished session: response 1 settled from U, `unknown` more responses
/// (2, 3, ...) without usage; the session is `unknown`.
async fn unknown_session(
    f: &Fixture,
    principal: crate::auth::Principal,
    d: &Deployment,
    unknown: i32,
) -> Uuid {
    let id = open_session(f, principal, d).await;
    respond(f, &principal, id, 1, Some(U)).await;
    for n in 2..2 + unknown {
        respond(f, &principal, id, n, None).await;
    }
    end(f, id).await;
    assert_eq!(amounts(f, id).await.0, "unknown");
    id
}
async fn ledger(f: &Fixture, id: Uuid) -> Vec<(String, Option<i64>)> {
    sqlx::query_as(
        "SELECT kind,amount_microusd FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind",
    )
    .bind(id)
    .fetch_all(&f.store.pool)
    .await
    .unwrap()
}
async fn audits(f: &Fixture) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='realtime_response.reconciled'",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap()
}
async fn response(f: &Fixture, id: Uuid, n: i32) -> (String, Option<i64>, Option<String>) {
    sqlx::query_as("SELECT state,actual_microusd,evidence FROM realtime_responses WHERE execution_id=$1 AND sequence=$2")
        .bind(id)
        .bind(n)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn resolving_the_last_unknown_response_settles_the_session_once(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = unknown_session(&f, f.team, &d, 2).await;
    let (_, held, _, _) = amounts(&f, id).await;
    assert_eq!(held, Some(RESPONSE + 2 * WINDOW));
    let ws = f.team.workspace_id;
    // The first of two unknown responses: it settles, the session stays
    // unknown and keeps its whole hold.
    assert_eq!(
        resolve_response(&f.store, ws, id, 2, U, "invoice:2", f.owner).await,
        Ok(Resolved::Response)
    );
    assert_eq!(
        response(&f, id, 2).await,
        ("settled".into(), Some(RESPONSE), Some("invoice:2".into()))
    );
    assert_eq!(amounts(&f, id).await, ("unknown".into(), held, None, false));
    // The last one settles the session: the sum of its responses.
    assert_eq!(
        resolve_response(&f.store, ws, id, 3, U, "invoice:3", f.owner).await,
        Ok(Resolved::Session)
    );
    assert_eq!(
        amounts(&f, id).await,
        ("settled".into(), held, Some(3 * RESPONSE), false)
    );
    let unknown_amount = ledger(&f, id)
        .await
        .iter()
        .find(|r| r.0 == "unknown")
        .unwrap()
        .1;
    assert_eq!(
        ledger(&f, id).await,
        [
            ("hold".into(), ledger(&f, id).await[0].1),
            ("reconciliation".into(), Some(3 * RESPONSE)),
            ("unknown".into(), unknown_amount),
        ]
    );
    let totals: (String, String) = sqlx::query_as("SELECT settled_microusd::text,held_microusd::text FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime'")
        .bind(ws).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(totals, ((3 * RESPONSE).to_string(), "0".into()));
    let execution: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions WHERE id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(execution, (Some(3 * 30), Some(3 * 50)));
    assert_eq!(audits(&f).await, 2);
    // The identical resolution again writes nothing; other evidence for a
    // settled response, or resolving the response that settled live, is
    // refused.
    assert_eq!(
        resolve_response(&f.store, ws, id, 3, U, "invoice:3", f.owner).await,
        Ok(Resolved::Replay)
    );
    for (n, evidence) in [(3, "invoice:other"), (1, "invoice:1")] {
        assert_eq!(
            resolve_response(&f.store, ws, id, n, U, evidence, f.owner).await,
            Err(InferenceError::InvalidRequest)
        );
    }
    assert_eq!(ledger(&f, id).await.len(), 3);
    assert_eq!(audits(&f).await, 2);
}

#[dbtest::test]
async fn evidence_never_lowers_what_a_response_recorded(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = unknown_session(&f, f.team, &d, 1).await;
    // An unknown response that recorded some counters (here: inserted after
    // the session ended, as a fixture).
    sqlx::query("INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,input_text_tokens,output_audio_tokens,completed_at) VALUES($1,3,'unknown',0,10,40,clock_timestamp())")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let ws = f.team.workspace_id;
    for lower in [
        RealtimeUsage {
            input_text_tokens: 9,
            cached_text_tokens: 4,
            ..U
        },
        RealtimeUsage {
            output_audio_tokens: 39,
            ..U
        },
    ] {
        assert_eq!(
            resolve_response(&f.store, ws, id, 3, lower, "invoice", f.owner).await,
            Err(InferenceError::InvalidRequest)
        );
    }
    // Cached above input, blank or overlong evidence, a missing response.
    assert_eq!(
        resolve_response(
            &f.store,
            ws,
            id,
            2,
            RealtimeUsage {
                cached_text_tokens: 11,
                ..U
            },
            "invoice",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    for evidence in ["  ", &"x".repeat(201), "line\nbreak"] {
        assert_eq!(
            resolve_response(&f.store, ws, id, 3, U, evidence, f.owner).await,
            Err(InferenceError::InvalidRequest)
        );
    }
    assert_eq!(
        resolve_response(&f.store, ws, id, 9, U, "invoice", f.owner).await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(response(&f, id, 3).await.0, "unknown");
    // Raising a recorded cached count lowers the recorded uncached input.
    sqlx::query("INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,input_text_tokens,cached_text_tokens,completed_at) VALUES($1,4,'unknown',0,10,4,clock_timestamp())")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        resolve_response(
            &f.store,
            ws,
            id,
            4,
            RealtimeUsage {
                cached_text_tokens: 5,
                ..U
            },
            "invoice",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    // Equal or higher counts settle it (response 2 is still unknown).
    assert_eq!(
        resolve_response(&f.store, ws, id, 3, U, "invoice", f.owner).await,
        Ok(Resolved::Response)
    );
}

#[dbtest::test]
async fn nothing_is_resolved_while_the_session_is_open(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let open = open_session(&f, f.team, &d).await;
    respond(&f, &f.team, open, 1, None).await;
    assert_eq!(
        resolve_response(
            &f.store,
            f.team.workspace_id,
            open,
            1,
            U,
            "invoice",
            f.owner
        )
        .await,
        Err(InferenceError::Busy)
    );
    assert_eq!(amounts(&f, open).await.0, "pending");
    assert_eq!(response(&f, open, 1).await.0, "unknown");
}

/// Without a pinned price nothing proves a cost, not even zero.
#[dbtest::test]
async fn an_unpriced_session_is_never_resolved(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, false).await;
    let id = open_session(&f, f.team, &d).await;
    respond(&f, &f.team, id, 1, None).await;
    end(&f, id).await;
    assert_eq!(amounts(&f, id).await.0, "unknown");
    assert_eq!(
        resolve_response(&f.store, f.team.workspace_id, id, 1, U, "invoice", f.owner).await,
        Err(InferenceError::Configuration)
    );
    assert_eq!(response(&f, id, 1).await.0, "unknown");
    assert_eq!(amounts(&f, id).await.0, "unknown");
}

#[dbtest::test]
async fn only_a_live_platform_admin_resolves_and_personal_sessions_only_their_owner(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let shared = unknown_session(&f, f.team, &d, 1).await;
    // A member without the platform Admin role.
    assert_eq!(
        resolve_response(
            &f.store,
            f.team.workspace_id,
            shared,
            2,
            U,
            "invoice",
            f.other
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    // Another Platform Admin cannot resolve the owner's personal session.
    let personal = unknown_session(&f, f.principal, &d, 1).await;
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual') ON CONFLICT DO NOTHING")
        .bind(f.other)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        resolve_response(
            &f.store,
            f.principal.workspace_id,
            personal,
            2,
            U,
            "invoice",
            f.other
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    // The wrong workspace for the session.
    assert_eq!(
        resolve_response(
            &f.store,
            f.team.workspace_id,
            personal,
            2,
            U,
            "invoice",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(response(&f, personal, 2).await.0, "unknown");
    // Its owner, a Platform Admin, can.
    assert_eq!(
        resolve_response(
            &f.store,
            f.principal.workspace_id,
            personal,
            2,
            U,
            "invoice",
            f.owner
        )
        .await,
        Ok(Resolved::Session)
    );
    assert_eq!(audits(&f).await, 1);
}

#[dbtest::test]
async fn concurrent_resolutions_of_one_response_settle_it_once(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = unknown_session(&f, f.team, &d, 1).await;
    let ws = f.team.workspace_id;
    let (a, b) = tokio::join!(
        resolve_response(&f.store, ws, id, 2, U, "invoice:a", f.owner),
        resolve_response(&f.store, ws, id, 2, U, "invoice:b", f.owner),
    );
    let mut results = [a, b];
    results.sort_by_key(|r| r.is_err());
    assert_eq!(
        results,
        [Ok(Resolved::Session), Err(InferenceError::InvalidRequest)]
    );
    let reconciliations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'",
    )
    .bind(id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(reconciliations, 1);
    assert_eq!(amounts(&f, id).await.2, Some(2 * RESPONSE));
}

#[dbtest::test]
async fn a_settled_response_without_components_leaves_the_session_without_them(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = unknown_session(&f, f.team, &d, 1).await;
    // A settled response whose components were not recorded (the schema
    // allows it): the session's components could not total its actual.
    sqlx::query("INSERT INTO realtime_responses(execution_id,sequence,state,status,window_hold_microusd,actual_microusd,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,completed_at) VALUES($1,3,'settled','completed',0,7,0,0,0,0,0,0,clock_timestamp())")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        resolve_response(&f.store, f.team.workspace_id, id, 2, U, "invoice", f.owner).await,
        Ok(Resolved::Session)
    );
    let session: (String, Option<i64>, Option<Value>) = sqlx::query_as(
        "SELECT state,actual_microusd,cost_components FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(session, ("settled".into(), Some(2 * RESPONSE + 7), None));
}

/// A session whose process died (lease expiry), reserving as
/// `RealtimeSession` does (the first request resizes admission's window):
/// response 1 done with `first` (unknown without usage), response 2 open;
/// with `armed`, request 3's window reserved (and perhaps forwarded) but never
/// opened as a response row.
async fn expired_session(
    f: &Fixture,
    principal: crate::auth::Principal,
    d: &Deployment,
    first: Option<RealtimeUsage>,
    armed: bool,
) -> Uuid {
    let id = open_session(f, principal, d).await;
    let admission = Some(ResponseBound::admission(100));
    realtime::reserve_window(&f.store, &principal, id, "company/smart", FULL, admission)
        .await
        .unwrap();
    realtime::open_response(&f.store, id, 1, FULL)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        id,
        1,
        Some(ResponseStatus::Completed),
        first,
        FULL,
    )
    .await
    .unwrap();
    realtime::reserve_window(&f.store, &principal, id, "company/smart", FULL, None)
        .await
        .unwrap();
    realtime::open_response(&f.store, id, 2, FULL)
        .await
        .unwrap();
    if armed {
        realtime::reserve_window(&f.store, &principal, id, "company/smart", FULL, None)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(reconcile_expired(&f.store, 10).await.unwrap(), 1);
    assert_eq!(response(f, id, 2).await.0, "unknown");
    id
}

#[dbtest::test]
async fn a_lease_expired_session_settles_once_its_rows_explain_its_hold(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = expired_session(&f, f.team, &d, Some(U), false).await;
    assert_eq!(amounts(&f, id).await.1, Some(RESPONSE + WINDOW));
    assert_eq!(
        resolve_response(
            &f.store,
            f.team.workspace_id,
            id,
            2,
            U,
            "invoice:2",
            f.owner
        )
        .await,
        Ok(Resolved::Session)
    );
    assert_eq!(amounts(&f, id).await.2, Some(2 * RESPONSE));
}

#[dbtest::test]
async fn a_lease_expired_session_with_two_unknown_responses_settles_after_both(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = expired_session(&f, f.team, &d, None, false).await;
    assert_eq!(amounts(&f, id).await.1, Some(2 * WINDOW));
    let ws = f.team.workspace_id;
    // Each resolution values its response below the window it held; the
    // session's hold is still exactly what its rows retained.
    assert_eq!(
        resolve_response(&f.store, ws, id, 1, U, "invoice:1", f.owner).await,
        Ok(Resolved::Response)
    );
    assert_eq!(
        resolve_response(&f.store, ws, id, 2, U, "invoice:2", f.owner).await,
        Ok(Resolved::Session)
    );
    assert_eq!(amounts(&f, id).await.2, Some(2 * RESPONSE));
}

#[dbtest::test]
async fn a_window_without_a_response_row_keeps_the_session_unknown(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = expired_session(&f, f.team, &d, None, true).await;
    let before = amounts(&f, id).await;
    // Request 3's window is held but has no row: the provider may have run it.
    assert_eq!(before.1, Some(3 * WINDOW));
    let ws = f.team.workspace_id;
    // Response 1 is valued at exactly its window plus the unrowed one (5,600
    // text output tokens at $16/M): the settled values plus the remaining
    // window then add up to the hold, which still proves nothing.
    let two_windows = RealtimeUsage {
        input_text_tokens: 0,
        cached_text_tokens: 0,
        input_audio_tokens: 0,
        cached_audio_tokens: 0,
        output_text_tokens: 5_600,
        output_audio_tokens: 0,
    };
    for (n, usage) in [(1, two_windows), (2, U)] {
        assert_eq!(
            resolve_response(&f.store, ws, id, n, usage, "invoice", f.owner).await,
            Ok(Resolved::Response)
        );
        assert_eq!(response(&f, id, n).await.0, "settled");
    }
    assert_eq!(response(&f, id, 1).await.1, Some(2 * WINDOW));
    assert_eq!(amounts(&f, id).await, before);
    assert!(ledger(&f, id).await.iter().all(|l| l.0 != "reconciliation"));
    let settled: Vec<bool> = sqlx::query_scalar("SELECT (metadata->>'session_settled')::boolean FROM audit_events WHERE action='realtime_response.reconciled' AND resource_id=$1")
        .bind(id)
        .fetch_all(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(settled, [false, false]);
}

/// The response guard (0090) refuses every other change of a resolved or
/// terminal response, whoever writes it.
#[dbtest::test]
async fn a_resolved_response_never_changes_again(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = unknown_session(&f, f.team, &d, 2).await;
    resolve_response(&f.store, f.team.workspace_id, id, 2, U, "invoice", f.owner)
        .await
        .unwrap();
    sqlx::query("INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,floor_microusd,unbounded_cost,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,completed_at) VALUES($1,4,'unknown',0,5000,true,10,4,0,0,0,0,clock_timestamp())")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (n, sql) in [
        (
            2,
            "UPDATE realtime_responses SET evidence='other' WHERE execution_id=$1 AND sequence=$2",
        ),
        (
            2,
            "UPDATE realtime_responses SET actual_microusd=0,cost_components=NULL WHERE execution_id=$1 AND sequence=$2",
        ),
        (
            1,
            "UPDATE realtime_responses SET evidence='late',resolved_at=now() WHERE execution_id=$1 AND sequence=$2",
        ),
        (
            3,
            "UPDATE realtime_responses SET state='settled',actual_microusd=0,input_text_tokens=0,cached_text_tokens=0,input_audio_tokens=0,cached_audio_tokens=0,output_text_tokens=0,output_audio_tokens=0 WHERE execution_id=$1 AND sequence=$2",
        ),
        // Response 4 (below): unbounded, with a known floor and recorded
        // text input. A resolution keeps the unbounded mark, never values it
        // below its floor and never lowers its uncached input.
        (
            4,
            "UPDATE realtime_responses SET state='settled',actual_microusd=6000,unbounded_cost=false,evidence='e',resolved_at=now() WHERE execution_id=$1 AND sequence=$2",
        ),
        (
            4,
            "UPDATE realtime_responses SET state='settled',actual_microusd=100,evidence='e',resolved_at=now() WHERE execution_id=$1 AND sequence=$2",
        ),
        (
            4,
            "UPDATE realtime_responses SET state='settled',actual_microusd=6000,cached_text_tokens=8,evidence='e',resolved_at=now() WHERE execution_id=$1 AND sequence=$2",
        ),
        (
            4,
            "UPDATE realtime_responses SET state='settled',actual_microusd=6000,input_text_tokens=NULL,evidence='e',resolved_at=now() WHERE execution_id=$1 AND sequence=$2",
        ),
    ] {
        let error = sqlx::query(sql)
            .bind(id)
            .bind(n)
            .execute(&f.store.pool)
            .await
            .expect_err(sql);
        assert!(
            error.to_string().contains("realtime responses settle once"),
            "{sql}: {error}"
        );
    }
}

// Session-level resolution (`resolve_session`): a finished session none of
// whose responses is unknown.

/// Lease expiry of `id` (its process stopped).
async fn expire(f: &Fixture, id: Uuid) {
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(reconcile_expired(&f.store, 10).await.unwrap(), 1);
}
/// A session killed while idle, reserving as `RealtimeSession` does:
/// response 1 settled from U by `response.done`; with `armed`, request 2's
/// window reserved (and perhaps forwarded) with no response row.
async fn idle_session(
    f: &Fixture,
    principal: crate::auth::Principal,
    d: &Deployment,
    armed: bool,
) -> Uuid {
    idle_session_with(f, principal, d, U, armed).await
}
async fn idle_session_with(
    f: &Fixture,
    principal: crate::auth::Principal,
    d: &Deployment,
    first: RealtimeUsage,
    armed: bool,
) -> Uuid {
    let id = open_session(f, principal, d).await;
    let admission = Some(ResponseBound::admission(100));
    realtime::reserve_window(&f.store, &principal, id, "company/smart", FULL, admission)
        .await
        .unwrap();
    realtime::open_response(&f.store, id, 1, FULL)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        id,
        1,
        Some(ResponseStatus::Completed),
        Some(first),
        FULL,
    )
    .await
    .unwrap();
    if armed {
        realtime::reserve_window(&f.store, &principal, id, "company/smart", FULL, None)
            .await
            .unwrap();
    }
    expire(f, id).await;
    id
}
async fn session_audits(f: &Fixture, id: Uuid) -> Vec<Value> {
    sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='realtime_session.reconciled' AND resource_id=$1")
        .bind(id)
        .fetch_all(&f.store.pool)
        .await
        .unwrap()
}
const ZERO: RealtimeUsage = RealtimeUsage {
    input_text_tokens: 0,
    cached_text_tokens: 0,
    input_audio_tokens: 0,
    cached_audio_tokens: 0,
    output_text_tokens: 0,
    output_audio_tokens: 0,
};

#[dbtest::test]
async fn an_idle_killed_session_settles_from_evidence_once(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = idle_session(&f, f.team, &d, false).await;
    let ws = f.team.workspace_id;
    assert_eq!(
        amounts(&f, id).await,
        ("unknown".into(), Some(RESPONSE), None, false)
    );
    // No response is unknown: the per-response route has nothing to act on.
    assert_eq!(
        resolve_response(&f.store, ws, id, 1, U, "invoice", f.owner).await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(
        resolve_session(&f.store, ws, id, None, "invoice:s", f.owner).await,
        Ok(Ok(Resolved::Session))
    );
    assert_eq!(amounts(&f, id).await.2, Some(RESPONSE));
    assert_eq!(amounts(&f, id).await.0, "settled");
    assert!(
        ledger(&f, id)
            .await
            .contains(&("reconciliation".into(), Some(RESPONSE)))
    );
    assert_eq!(
        session_audits(&f, id).await,
        [json!({"remainder_sequence": null})]
    );
    // The identical request again writes nothing; anything else is refused.
    assert_eq!(
        resolve_session(&f.store, ws, id, None, "invoice:s", f.owner).await,
        Ok(Ok(Resolved::Replay))
    );
    for (remainder, evidence) in [(None, "invoice:other"), (Some(ZERO), "invoice:s")] {
        assert_eq!(
            resolve_session(&f.store, ws, id, remainder, evidence, f.owner).await,
            Err(InferenceError::InvalidRequest)
        );
    }
    assert_eq!(ledger(&f, id).await.len(), 3);
    assert_eq!(session_audits(&f, id).await.len(), 1);
}

#[dbtest::test]
async fn an_unrowed_window_is_settled_from_its_stated_usage(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let ws = f.team.workspace_id;
    for (usage, value) in [(ZERO, 0), (U, RESPONSE)] {
        let id = idle_session(&f, f.team, &d, true).await;
        let before = amounts(&f, id).await;
        assert_eq!(before.1, Some(RESPONSE + WINDOW));
        // The hold holds request 2's window: its usage must be stated.
        assert_eq!(
            resolve_session(&f.store, ws, id, None, "invoice", f.owner).await,
            Ok(Err(SessionRefusal::RemainderRequired))
        );
        assert_eq!(amounts(&f, id).await, before);
        assert_eq!(
            resolve_session(&f.store, ws, id, Some(usage), "invoice:rest", f.owner).await,
            Ok(Ok(Resolved::Session))
        );
        assert_eq!(
            response(&f, id, 2).await,
            ("settled".into(), Some(value), Some("invoice:rest".into()))
        );
        let window: Option<i64> = sqlx::query_scalar("SELECT window_hold_microusd FROM realtime_responses WHERE execution_id=$1 AND sequence=2")
            .bind(id).fetch_one(&f.store.pool).await.unwrap();
        assert_eq!(window, Some(WINDOW));
        assert_eq!(
            amounts(&f, id).await,
            ("settled".into(), before.1, Some(RESPONSE + value), false)
        );
        assert!(
            ledger(&f, id)
                .await
                .contains(&("reconciliation".into(), Some(RESPONSE + value)))
        );
        assert_eq!(
            session_audits(&f, id).await,
            [json!({"remainder_sequence": 2})]
        );
        assert_eq!(
            resolve_session(&f.store, ws, id, Some(usage), "invoice:rest", f.owner).await,
            Ok(Ok(Resolved::Replay))
        );
    }
}

#[dbtest::test]
async fn a_session_killed_before_its_first_response_settles_from_the_stated_request(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = open_session(&f, f.team, &d).await;
    realtime::reserve_window(
        &f.store,
        &f.team,
        id,
        "company/smart",
        FULL,
        Some(ResponseBound::admission(100)),
    )
    .await
    .unwrap();
    expire(&f, id).await;
    assert_eq!(amounts(&f, id).await.1, Some(WINDOW));
    assert_eq!(
        resolve_session(
            &f.store,
            f.team.workspace_id,
            id,
            Some(U),
            "invoice",
            f.owner
        )
        .await,
        Ok(Ok(Resolved::Session))
    );
    assert_eq!(
        response(&f, id, 1).await,
        ("settled".into(), Some(RESPONSE), Some("invoice".into()))
    );
    assert_eq!(amounts(&f, id).await.2, Some(RESPONSE));
}

#[dbtest::test]
async fn a_session_resolution_refuses_what_it_cannot_prove(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let ws = f.team.workspace_id;
    // Still open.
    let open = open_session(&f, f.team, &d).await;
    respond(&f, &f.team, open, 1, Some(U)).await;
    assert_eq!(
        resolve_session(&f.store, ws, open, None, "invoice", f.owner).await,
        Ok(Err(SessionRefusal::Open))
    );
    // A response still unknown is resolved first, one by one.
    let unknown = unknown_session(&f, f.team, &d, 1).await;
    assert_eq!(
        resolve_session(&f.store, ws, unknown, None, "invoice", f.owner).await,
        Ok(Err(SessionRefusal::UnknownResponses))
    );
    // A remainder with no window to attribute it to.
    let idle = idle_session(&f, f.team, &d, false).await;
    assert_eq!(
        resolve_session(&f.store, ws, idle, Some(ZERO), "invoice", f.owner).await,
        Err(InferenceError::InvalidRequest)
    );
    // Not a live Platform Admin, the wrong workspace, invalid evidence or usage.
    for (workspace, actor, evidence, remainder) in [
        (ws, f.other, "invoice", None),
        (f.principal.workspace_id, f.owner, "invoice", None),
        (ws, f.owner, "line\nbreak", None),
        (
            ws,
            f.owner,
            "invoice",
            Some(RealtimeUsage {
                cached_text_tokens: 11,
                ..U
            }),
        ),
    ] {
        assert_eq!(
            resolve_session(&f.store, workspace, idle, remainder, evidence, actor).await,
            Err(InferenceError::InvalidRequest)
        );
    }
    for id in [open, unknown, idle] {
        assert_ne!(amounts(&f, id).await.0, "settled");
        assert!(session_audits(&f, id).await.is_empty());
    }
}

#[dbtest::test]
async fn a_remainder_without_a_complete_valuation_is_refused(pool: PgPool) {
    let f = fixture(pool).await;
    // Audio output is not applicable on this price: audio output usage has no rate.
    let mut price = lines();
    for line in price.as_array_mut().unwrap() {
        if line["meter"] == "output_audio_tokens" {
            *line = na("output_audio_tokens");
        }
    }
    let d = realtime_model_priced(&f, Some(price)).await;
    let text_only = RealtimeUsage {
        output_audio_tokens: 0,
        ..U
    };
    let id = idle_session_with(&f, f.team, &d, text_only, true).await;
    let ws = f.team.workspace_id;
    assert_eq!(
        resolve_session(&f.store, ws, id, Some(U), "invoice", f.owner).await,
        Err(InferenceError::Configuration)
    );
    assert_eq!(amounts(&f, id).await.0, "unknown");
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM realtime_responses WHERE execution_id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
}

#[dbtest::test]
async fn an_unpriced_session_is_never_settled_by_evidence(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, false).await;
    let id = open_session(&f, f.team, &d).await;
    expire(&f, id).await;
    for remainder in [None, Some(ZERO)] {
        assert_eq!(
            resolve_session(
                &f.store,
                f.team.workspace_id,
                id,
                remainder,
                "invoice",
                f.owner
            )
            .await,
            Ok(Err(SessionRefusal::Unattributed))
        );
    }
    assert_eq!(amounts(&f, id).await.0, "unknown");
}

#[dbtest::test]
async fn an_unbounded_session_states_its_remainder_even_when_its_hold_matches(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let id = expired_session(&f, f.team, &d, Some(U), false).await;
    // A window the price could not bound added nothing to the hold and only
    // marked the session unbounded: the hold no longer proves it complete.
    sqlx::query("UPDATE governance_reservations SET unbounded_cost=true WHERE execution_id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let ws = f.team.workspace_id;
    assert_eq!(
        resolve_response(&f.store, ws, id, 2, U, "invoice:2", f.owner).await,
        Ok(Resolved::Response)
    );
    assert_eq!(
        resolve_session(&f.store, ws, id, None, "invoice:s", f.owner).await,
        Ok(Err(SessionRefusal::RemainderRequired))
    );
    assert_eq!(
        resolve_session(&f.store, ws, id, Some(ZERO), "invoice:s", f.owner).await,
        Ok(Ok(Resolved::Session))
    );
    let row: (Option<i64>, Option<i64>) = sqlx::query_as("SELECT window_hold_microusd,actual_microusd FROM realtime_responses WHERE execution_id=$1 AND sequence=3")
        .bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row, (Some(0), Some(0)));
    assert_eq!(amounts(&f, id).await.2, Some(2 * RESPONSE));
}

#[dbtest::test]
async fn concurrent_session_resolutions_settle_it_once(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, true).await;
    let ws = f.team.workspace_id;
    for (a, b, other) in [
        ("invoice", "invoice", Ok(Ok(Resolved::Replay))),
        (
            "invoice:a",
            "invoice:b",
            Err(InferenceError::InvalidRequest),
        ),
    ] {
        let id = idle_session(&f, f.team, &d, true).await;
        let (x, y) = tokio::join!(
            resolve_session(&f.store, ws, id, Some(U), a, f.owner),
            resolve_session(&f.store, ws, id, Some(U), b, f.owner),
        );
        let mut results = [x, y];
        results.sort_by_key(|r| *r != Ok(Ok(Resolved::Session)));
        assert_eq!(results, [Ok(Ok(Resolved::Session)), other]);
        let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'),(SELECT count(*) FROM realtime_responses WHERE execution_id=$1)")
            .bind(id).fetch_one(&f.store.pool).await.unwrap();
        assert_eq!(counts, (1, 2));
        assert_eq!(amounts(&f, id).await.2, Some(2 * RESPONSE));
    }
}
