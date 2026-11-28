//! #841: content a rule cannot read, end to end (real PostgreSQL and the
//! HTTP router). A Block rule that refuses what it cannot read
//! (`uninspectable: deny`) refuses every request of a workload this release
//! never reads (images, transcription, realtime, before the WebSocket
//! upgrade) and every generation request with an image, before anything is
//! admitted or sent; one that lets it through (`allow`) records the
//! decision as skipped and not refused, and still reads the text beside an
//! image.
use std::sync::atomic::Ordering;

use base64::Engine;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::guardrail_db_tests::{
    PNG, World, chat_with_image, images_on, secret, send, type_default_on, world, world_on,
};

/// The decisions recorded: operation, action and whether each refused.
async fn decisions(w: &World) -> Vec<(String, String, bool)> {
    sqlx::query_as("SELECT operation,action,refused FROM guardrail_events ORDER BY created_at,id")
        .fetch_all(&w.f.store.pool)
        .await
        .unwrap()
}

async fn realtime(w: &World) -> (StatusCode, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(format!("{}/v1/realtime?model=company/smart", w.base))
        .bearer_auth(&w.token)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        // A handshake nonce: any 16 bytes, base64-encoded.
        .header(
            "sec-websocket-key",
            base64::engine::general_purpose::STANDARD.encode([7u8; 16]),
        )
        .header("sec-websocket-protocol", "realtime")
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[dbtest::test]
async fn a_block_rule_naming_images_refuses_every_image_request_unless_it_lets_them_through(
    pool: PgPool,
) {
    let w = world(pool, "'images'").await;
    // An image model, so the route serves the request (the guardrail step
    // decides before anything is sent).
    sqlx::query("UPDATE deployments SET upstream_model='gpt-image-1' WHERE enabled")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    type_default_on(&w, "block", "{secret}", &[], &[], "{images}", "deny").await;
    let image = json!({"model":"company/smart","prompt":"a lighthouse at dusk"});
    let (status, retry, body) = send(&w, "/v1/images/generations", image.clone()).await;
    assert_eq!(
        (status, retry.as_deref()),
        (StatusCode::BAD_REQUEST, Some("false"))
    );
    assert_eq!(body["error"]["code"], "guardrail_not_supported");
    assert_eq!(w.hits.load(Ordering::SeqCst), 0, "nothing was sent");
    assert_eq!(
        decisions(&w).await,
        [("images".into(), "skipped".into(), true)]
    );
    // Let through unread: sent, and recorded as skipped, not refused.
    type_default_on(&w, "block", "{secret}", &[], &[], "{images}", "allow").await;
    let (_, _, body) = send(&w, "/v1/images/generations", image).await;
    assert_ne!(body["error"]["code"], "guardrail_not_supported", "{body}");
    assert_eq!(w.hits.load(Ordering::SeqCst), 1, "sent upstream");
    assert_eq!(
        decisions(&w).await[1..],
        [("images".into(), "skipped".into(), false)]
    );
}

#[dbtest::test]
async fn a_block_rule_naming_realtime_refuses_the_session_before_the_upgrade(pool: PgPool) {
    let w = world(pool, "'realtime'").await;
    type_default_on(&w, "block", "{secret}", &[], &[], "{realtime}", "deny").await;
    let (status, body) = realtime(&w).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no upgrade");
    assert_eq!(body["error"]["code"], "guardrail_not_supported");
    assert_eq!(w.hits.load(Ordering::SeqCst), 0, "nothing was sent");
    let admitted: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(admitted, 0, "nothing was admitted");
    assert_eq!(
        decisions(&w).await,
        [("realtime".into(), "skipped".into(), true)]
    );
    // Let through unread: the session goes on past the guardrail step.
    type_default_on(&w, "block", "{secret}", &[], &[], "{realtime}", "allow").await;
    let (_, body) = realtime(&w).await;
    assert_ne!(body["error"]["code"], "guardrail_not_supported", "{body}");
    assert_eq!(
        decisions(&w).await[1..],
        [("realtime".into(), "skipped".into(), false)]
    );
}

#[dbtest::test]
async fn a_block_rule_naming_transcription_refuses_every_transcription(pool: PgPool) {
    let w = world(pool, "'audio_transcriptions'").await;
    type_default_on(
        &w,
        "block",
        "{secret}",
        &[],
        &[],
        "{audio_transcriptions}",
        "deny",
    )
    .await;
    // A multipart form with a model and a file, assembled here.
    let boundary = "omg-unread-boundary";
    let transcribe = async || {
        let form = format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\ncompany/smart\r\n--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\ncontent-type: audio/wav\r\n\r\nRIFF....WAVE\r\n--{boundary}--\r\n"
        );
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/v1/audio/transcriptions", w.base))
            .bearer_auth(&w.token)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(form)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            response.json::<Value>().await.unwrap_or(Value::Null),
        )
    };
    let (status, body) = transcribe().await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "guardrail_not_supported");
    assert_eq!(w.hits.load(Ordering::SeqCst), 0, "nothing was sent");
    assert_eq!(
        decisions(&w).await,
        [("audio_transcriptions".into(), "skipped".into(), true)]
    );
    type_default_on(
        &w,
        "block",
        "{secret}",
        &[],
        &[],
        "{audio_transcriptions}",
        "allow",
    )
    .await;
    let (_, body) = transcribe().await;
    assert_ne!(body["error"]["code"], "guardrail_not_supported", "{body}");
    assert_eq!(
        decisions(&w).await[1..],
        [("audio_transcriptions".into(), "skipped".into(), false)]
    );
}

#[dbtest::test]
async fn under_allow_an_image_goes_on_with_its_text_still_read(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    images_on(&w).await;
    type_default_on(
        &w,
        "block",
        "{secret}",
        &[],
        &[],
        "{generation,embeddings}",
        "allow",
    )
    .await;
    let (status, _, _) = send(&w, "/v1/chat/completions", chat_with_image("what is this?")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        decisions(&w).await,
        [("chat_completions".into(), "skipped".into(), false)]
    );
    // A secret beside the image is still found, and refuses.
    let hits = w.hits.load(Ordering::SeqCst);
    let (status, _, body) = send(
        &w,
        "/v1/chat/completions",
        chat_with_image(&format!("use {}", secret())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
    assert_eq!(w.hits.load(Ordering::SeqCst), hits, "nothing was sent");
}

/// A token count with an image goes on under `allow` (it is a generation
/// request, its text read) and is recorded as skipped, not refused.
#[dbtest::test]
async fn under_allow_a_token_count_with_an_image_goes_on(pool: PgPool) {
    let w = world_on(pool, "'messages'", "anthropic").await;
    images_on(&w).await;
    let body = || {
        let data = PNG.split_once(',').unwrap().1;
        json!({"model":"company/smart","messages":[{"role":"user","content":[
            {"type":"text","text":"how many tokens?"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}]}]})
    };
    type_default_on(&w, "block", "{secret}", &[], &[], "{generation}", "deny").await;
    let (_, _, refused) = send(&w, "/v1/messages/count_tokens", body()).await;
    assert_eq!(
        refused["error"]["guardrail"]["category"], "secret",
        "{refused}"
    );
    type_default_on(&w, "block", "{secret}", &[], &[], "{generation}", "allow").await;
    let (_, _, value) = send(&w, "/v1/messages/count_tokens", body()).await;
    assert!(value["error"]["guardrail"].is_null(), "{value}");
    assert_eq!(
        decisions(&w).await,
        [
            ("count_tokens".into(), "skipped".into(), true),
            ("count_tokens".into(), "skipped".into(), false)
        ]
    );
}

/// 0100 stops, applying nothing, when the CHECK it replaces is not found
/// exactly once (a database whose tightening CHECK was changed by hand).
#[sqlx::test(migrations = false)]
async fn the_uninspectable_migration_stops_on_an_unexpected_check(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    let through = |last: i64| sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= last).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    through(99).run(&pool).await.unwrap();
    // A second CHECK of the same shape.
    sqlx::query("ALTER TABLE guardrail_rules ADD CONSTRAINT hand_made CHECK (tightens_rule_id IS NULL OR (name IS NULL AND uninspectable IS NULL))")
        .execute(&pool)
        .await
        .unwrap();
    let err = through(i64::MAX).run(&pool).await.unwrap_err().to_string();
    assert!(
        err.contains("expected one tightening-entry CHECK on guardrail_rules, found 2"),
        "{err}"
    );
    let version: i64 =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, 99, "nothing of 0100 applied");
}

/// The CHECK that refused a write (its SQLSTATE is 23514).
fn constraint(r: Result<(), sqlx::Error>) -> String {
    match r {
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23514") => {
            e.constraint().unwrap_or_default().to_owned()
        }
        other => format!("{other:?}"),
    }
}

/// 0100 on a database at 0099 holding rules, a tightening and a decision:
/// they are kept and read as before; afterwards a tightening may refuse what
/// a platform rule lets through unread (and only that), and a realtime
/// decision is recorded in a partition that existed before the migration
/// and in one created after it.
#[sqlx::test(migrations = false)]
async fn the_uninspectable_migration_keeps_rules_tightenings_and_decisions(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    let through = |last: i64| sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= last).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    through(99).run(&pool).await.unwrap();
    let f = crate::governance::tests::db::fixture(pool.clone()).await;
    let (ws, key) = (f.principal.workspace_id, f.principal.key_id);
    let team = f.team.workspace_id;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(team)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (set, local, rule) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Secrets','secrets','pattern','{secret}','monitor','user_text','off','fail_open','{generation}','deny')")
        .bind(rule).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let writer: Uuid = sqlx::query_scalar("SELECT user_id FROM effective_workspace_memberships WHERE workspace_id=$1 AND role IN ('owner','admin') LIMIT 1")
        .bind(team).fetch_one(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,workspace_id,updated_by) VALUES($1,'workspace_local',$2,$3)")
        .bind(local).bind(team).bind(writer).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,input_action) VALUES($1,$2,0,$3,'block')")
        .bind(Uuid::now_v7()).bind(local).bind(rule).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let event = "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,created_at) VALUES($1,$2,$3,$4,$5,'company/smart','input',$6,$7,1,'type_default','pattern','secret','skipped',1,";
    let after = "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refuses,created_at) VALUES($1,$2,$3,$4,$5,'company/smart','input',$6,$7,1,'type_default','pattern','secret','skipped',1,false,";
    sqlx::query(&format!("{event}clock_timestamp())"))
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(key)
        .bind("images")
        .bind(set)
        .bind(rule)
        .execute(&pool)
        .await
        .unwrap();

    through(i64::MAX).run(&pool).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_rules")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2, "the rule and the tightening are kept");
    let mut conn = pool.acquire().await.unwrap();
    let read =
        crate::guardrails::stored::load(&mut conn, crate::guardrails::stored::Scope::Local(team))
            .await
            .unwrap()
            .expect("readable")
            .expect("the local layer");
    assert_eq!(read.rules.len(), 1);
    drop(conn);
    // A tightening may now set uninspectable, only to deny.
    let tightening = |position: i16, value: &'static str| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(
                "UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1",
            )
            .bind(local)
            .bind(writer)
            .execute(&mut *tx)
            .await
            .unwrap();
            let r = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,uninspectable) VALUES($1,$2,$3,$4,$5)")
                .bind(Uuid::now_v7()).bind(local).bind(position).bind(rule).bind(value)
                .execute(&mut *tx).await.map(|_| ());
            if r.is_ok() {
                tx.commit().await.unwrap();
            }
            r
        }
    };
    tightening(1, "deny").await.unwrap();
    assert_eq!(
        constraint(tightening(2, "allow").await),
        "guardrail_rules_tightening"
    );
    // Realtime decisions, in the existing month partition and in one
    // created after the migration; an unknown operation is refused.
    let created: Vec<(String, String)> = sqlx::query_as(
        "SELECT parent,partition FROM omg_ensure_partitions(0,clock_timestamp()+interval '14 months')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        created
            .iter()
            .any(|(parent, _)| parent == "guardrail_events")
    );
    for at in [
        "clock_timestamp())",
        "clock_timestamp()+interval '14 months')",
    ] {
        // Written from now on with its verdict (`guardrail_events_refuses`).
        let insert = |operation: &'static str| {
            let (pool, sql) = (pool.clone(), format!("{after}{at}"));
            async move {
                sqlx::query(&sql)
                    .bind(Uuid::now_v7())
                    .bind(Uuid::now_v7())
                    .bind(ws)
                    .bind(key)
                    .bind(operation)
                    .bind(set)
                    .bind(rule)
                    .execute(&pool)
                    .await
                    .map(|_| ())
            }
        };
        insert("realtime").await.expect(at);
        assert_eq!(
            constraint(insert("probe_operation").await),
            "guardrail_events_operation",
            "{at}"
        );
        // Without its verdict, or a refusal that does not refuse.
        let row = format!("{event}{at}");
        let bare = sqlx::query(&row)
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(ws)
            .bind(key)
            .bind("realtime")
            .bind(set)
            .bind(rule)
            .execute(&pool)
            .await
            .map(|_| ());
        assert_eq!(constraint(bare), "guardrail_events_refuses", "{at}");
    }
    let operations: Vec<(String, Option<bool>)> =
        sqlx::query_as("SELECT operation,refuses FROM guardrail_events ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        operations,
        [
            ("images".into(), None),
            ("realtime".into(), Some(false)),
            ("realtime".into(), Some(false))
        ],
        "a decision written before 0100 has no verdict"
    );
}
