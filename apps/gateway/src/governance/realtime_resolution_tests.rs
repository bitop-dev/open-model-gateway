//! #804: a Platform Admin resolves the unknown responses of a finished
//! realtime session from evidence (`realtime::resolve_response`), on a
//! disposable database. The refusals mirror request resolution
//! (`resolve_usage`): never zero without proof, never a second settlement,
//! no lowering of recorded usage, nothing while the session is open, live
//! Platform Admin authority, personal workspaces only for their owner.
use super::realtime::{Resolved, resolve_response};
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
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['realtime'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    if priced {
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,100,3,$3,'{}')")
            .bind(Uuid::new_v4()).bind(f.deployment).bind(lines()).execute(&f.store.pool).await.unwrap();
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
