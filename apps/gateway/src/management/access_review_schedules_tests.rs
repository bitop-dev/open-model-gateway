//! #481: access reviews started on a schedule, on real PostgreSQL. Runs are
//! exactly once per schedule and period across schedulers, a claimant that
//! dies leaves one late run, missed periods are not backfilled, skipped runs
//! are recorded and alert once, the scheduler acts as the system, and pausing
//! or deleting a schedule never touches its campaigns.
use super::*;
use crate::management::access_review_schedules::{Outcome, Skip, run_due_fenced};
use gateway_datetime_compat::UtcInstant;

const SCHEDULES: &str = "/api/v1/platform/access-review-schedules";
/// The first of this month and of the next, 00:00 UTC.
const THIS_MONTH: &str = "date_trunc('month',now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'";
const NEXT_MONTH: &str =
    "(date_trunc('month',now() AT TIME ZONE 'UTC') + interval '1 month') AT TIME ZONE 'UTC'";

fn monthly(f: &Fixture, name: &str) -> Value {
    json!({"name":name,"subjects":["workspace_memberships"],"workspace_ids":[f.team],"open_days":7,"cadence":"monthly","start_day":1})
}
async fn create(f: &Fixture, body: Value) -> Uuid {
    let (status, created) = call(&f.s, &f.admin, "POST", SCHEDULES, body).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    id(&created)
}
/// Makes a schedule due for the period that started on the 1st of `months_ago`
/// months back (0: this month).
async fn due_since(pool: &PgPool, schedule: Uuid, months_ago: i32) {
    sqlx::query("UPDATE access_review_schedules SET next_run_at=(date_trunc('month',now() AT TIME ZONE 'UTC') - make_interval(months=>$2)) AT TIME ZONE 'UTC' WHERE id=$1")
        .bind(schedule)
        .bind(months_ago)
        .execute(pool)
        .await
        .unwrap();
}
async fn scalar(pool: &PgPool, sql: &str, schedule: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(schedule)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn campaigns_of(pool: &PgPool, schedule: Uuid) -> i64 {
    scalar(
        pool,
        "SELECT count(*) FROM access_review_campaigns WHERE schedule_id=$1",
        schedule,
    )
    .await
}
async fn next_is_next_month(pool: &PgPool, schedule: Uuid) -> bool {
    sqlx::query_scalar(&format!(
        "SELECT next_run_at={NEXT_MONTH} FROM access_review_schedules WHERE id=$1"
    ))
    .bind(schedule)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[dbtest::test]
async fn two_schedulers_racing_start_one_campaign(pool: PgPool) {
    let f = fixture(&pool).await;
    for round in 0..3 {
        let schedule = create(&f, monthly(&f, &format!("Monthly team review {round}"))).await;
        due_since(&pool, schedule, 0).await;
        let other = Store::new(pool.clone());
        let (a, b) = tokio::join!(run_due_fenced(&f.s, None), run_due_fenced(&other, None));
        let outcomes: Vec<Outcome> = a
            .unwrap()
            .into_iter()
            .chain(b.unwrap())
            .filter(|(id, _)| *id == schedule)
            .map(|(_, o)| o)
            .collect();
        assert_eq!(outcomes.len(), 1, "round {round}: {outcomes:?}");
        assert!(matches!(outcomes[0], Outcome::Started(_)));
        assert_eq!(campaigns_of(&pool, schedule).await, 1);
        assert_eq!(
            scalar(
                &pool,
                "SELECT count(*) FROM access_review_schedule_runs WHERE schedule_id=$1",
                schedule
            )
            .await,
            1
        );
        assert!(next_is_next_month(&pool, schedule).await);
        // An ordinary campaign: the template, named for its period, created by
        // the schedule's admin and open for the template's days.
        let (name, created_by, open_days, items): (String, Uuid, i32, i64) = sqlx::query_as("SELECT c.name,c.created_by,extract(day FROM c.due_at-c.created_at)::int,(SELECT count(*) FROM access_review_items i WHERE i.campaign_id=c.id) FROM access_review_campaigns c WHERE c.schedule_id=$1")
            .bind(schedule)
            .fetch_one(&pool)
            .await
            .unwrap();
        let date: String = sqlx::query_scalar(&format!(
            "SELECT to_char({THIS_MONTH} AT TIME ZONE 'UTC','YYYY-MM-DD')"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(name, format!("Monthly team review {round} ({date})"));
        assert_eq!((created_by, open_days, items), (f.admin.user_id, 7, 2));
        // Closing it lets the next period start again; it is not reopened now.
        sqlx::query(
            "UPDATE access_review_campaigns SET closed_by=$2,closed_at=now() WHERE schedule_id=$1",
        )
        .bind(schedule)
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    }
}

#[dbtest::test]
async fn a_claimant_that_dies_before_committing_leaves_one_late_run(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Claimed")).await;
    due_since(&pool, schedule, 0).await;
    // The claimant takes the scheduler's locks, claims the schedule row and
    // starts the campaign, then dies before committing.
    let mut claimant = resources::installation_tx_fenced(&f.s, None).await.unwrap();
    sqlx::query("SELECT id FROM access_review_schedules WHERE id=$1 FOR UPDATE SKIP LOCKED")
        .bind(schedule)
        .fetch_one(&mut *claimant)
        .await
        .unwrap();
    sqlx::query(&format!(
        "SELECT omg_access_review_start_scheduled($1,$2,$3,{THIS_MONTH})"
    ))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(schedule)
    .execute(&mut *claimant)
    .await
    .unwrap();
    let other = Store::new(pool.clone());
    let waiting = tokio::spawn(async move { run_due_fenced(&other, None).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !waiting.is_finished(),
        "the next scheduler waits for the claimant"
    );
    assert_eq!(campaigns_of(&pool, schedule).await, 0);
    drop(claimant);
    let late = waiting.await.unwrap();
    assert_eq!(late.len(), 1, "{late:?}");
    assert!(matches!(late[0], (id, Outcome::Started(_)) if id == schedule));
    assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    assert_eq!(
        campaigns_of(&pool, schedule).await,
        1,
        "once, not twice and not never"
    );
}

#[dbtest::test]
async fn after_downtime_one_late_run_for_the_latest_period(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Downtime")).await;
    // Down since the period four months ago: one run, for this month's.
    due_since(&pool, schedule, 4).await;
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    assert_eq!(ran.len(), 1);
    assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    let this_month: bool = sqlx::query_scalar(&format!("SELECT bool_and(period_at={THIS_MONTH}) AND count(*)=1 FROM access_review_schedule_runs WHERE schedule_id=$1"))
        .bind(schedule)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(this_month, "no backfill");
    assert!(
        next_is_next_month(&pool, schedule).await,
        "the next period boundary, not now + 1 month"
    );
}

#[dbtest::test]
async fn a_skipped_run_is_recorded_alerts_once_and_never_queues(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Overlapping")).await;
    // Last month's campaign is still open.
    due_since(&pool, schedule, 1).await;
    sqlx::query(&format!(
        "SELECT omg_access_review_start_scheduled($1,$2,$3,{THIS_MONTH} - interval '1 month')"
    ))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(schedule)
    .execute(&pool)
    .await
    .unwrap();
    due_since(&pool, schedule, 0).await;
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    assert_eq!(ran, vec![(schedule, Outcome::Skipped(Skip::PreviousOpen))]);
    assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    assert_eq!(campaigns_of(&pool, schedule).await, 1);
    let alerts: Vec<(String, Value, Option<UtcInstant>)> = sqlx::query_as("SELECT e.subject_key,e.details,e.resolved_at FROM alert_events e WHERE e.builtin='access_review_skipped'")
        .fetch_all(&pool)
        .await
        .unwrap();
    let run: Uuid = sqlx::query_scalar("SELECT id FROM access_review_schedule_runs WHERE schedule_id=$1 AND outcome='skipped' AND reason='previous_open'")
        .bind(schedule)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1, "one alert per skipped run");
    assert_eq!(alerts[0].0, format!("access_review_run:{run}"));
    assert_eq!(alerts[0].1, json!({"reason":"previous_open"}));
    assert_eq!(
        scalar(&pool, "SELECT count(*) FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.builtin='access_review_skipped' AND d.transition='fired' AND $1::uuid IS NOT NULL", schedule).await,
        1
    );
    // Platform Admins see the incident; the schedule list shows the skip.
    let (_, events) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/alerts/events",
        Value::Null,
    )
    .await;
    assert!(
        events["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "access_review_skipped"),
        "{events}"
    );
    let (status, list) = call(&f.s, &f.auditor, "GET", SCHEDULES, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let row = &list["data"][0];
    assert_eq!(
        (
            row["last_run"]["outcome"].as_str(),
            row["last_run"]["reason"].as_str()
        ),
        (Some("skipped"), Some("previous_open"))
    );
    assert!(row["last_campaign"]["id"].is_string());
    // Closing the open campaign does not queue the skipped run.
    sqlx::query(
        "UPDATE access_review_campaigns SET closed_by=$2,closed_at=now() WHERE schedule_id=$1",
    )
    .bind(schedule)
    .bind(f.admin.user_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    assert_eq!(campaigns_of(&pool, schedule).await, 1);
    let audited: i64 = scalar(&pool, "SELECT count(*) FROM audit_events WHERE action='access_review_schedule.run_skipped' AND resource_id=$1 AND actor_user_id IS NULL AND metadata->>'reason'='previous_open'", schedule).await;
    assert_eq!(audited, 1);
    // Nothing to review is a skip too (and its own alert); the schedule's
    // incidents clear when it is deleted.
    let empty = create(&f, json!({"name":"Empty project","subjects":["workspace_memberships"],"workspace_ids":[workspace(&pool, "Nobody", "project", f.owner.user_id).await],"cadence":"monthly","start_day":1})).await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=(SELECT workspace_ids[1] FROM access_review_schedules WHERE id=$1)")
        .bind(empty)
        .execute(&pool)
        .await
        .unwrap();
    due_since(&pool, empty, 0).await;
    assert_eq!(
        run_due_fenced(&f.s, None).await.unwrap(),
        vec![(empty, Outcome::Skipped(Skip::Empty))]
    );
    let (status, _) = call(
        &f.s,
        &f.admin,
        "DELETE",
        &format!("{SCHEDULES}/{schedule}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let open: Vec<String> = sqlx::query_scalar("SELECT details->>'reason' FROM alert_events WHERE builtin='access_review_skipped' AND resolved_at IS NULL")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(open, ["empty"]);
}

#[dbtest::test]
async fn the_scheduler_acts_as_the_system_after_its_creator_loses_access(pool: PgPool) {
    let f = fixture(&pool).await;
    let creator = user(&pool, "creator", "admin").await;
    let (status, created) = call(&f.s, &creator, "POST", SCHEDULES, monthly(&f, "Demoted")).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let schedule = id(&created);
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(creator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(creator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    due_since(&pool, schedule, 0).await;
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    let Outcome::Started(campaign) = ran[0].1 else {
        panic!("{ran:?}")
    };
    let created_by: Uuid =
        sqlx::query_scalar("SELECT created_by FROM access_review_campaigns WHERE id=$1")
            .bind(campaign)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created_by, creator.user_id, "names the schedule's creator");
    let (actor, metadata): (Option<Uuid>, Value) = sqlx::query_as("SELECT actor_user_id,metadata FROM audit_events WHERE action='access_review.started_by_schedule' AND resource_id=$1")
        .bind(campaign)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(actor, None, "the system, not the creator");
    assert_eq!(
        metadata,
        json!({"schedule_id":schedule,"created_by":creator.user_id,"count":2})
    );
    // The demoted creator cannot start a campaign by hand, and nobody can
    // attach a campaign to a schedule outside the scheduler's function.
    assert_eq!(
        call(
            &f.s,
            &creator,
            "POST",
            "/api/v1/platform/access-reviews",
            json!({"name":"x","subjects":["platform_roles"]})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[dbtest::test]
async fn pausing_resuming_and_deleting_never_touch_past_campaigns(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Lifecycle")).await;
    due_since(&pool, schedule, 0).await;
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    let Outcome::Started(campaign) = ran[0].1 else {
        panic!("{ran:?}")
    };
    let snapshot = |pool: PgPool| async move {
        sqlx::query_scalar::<_, Value>(
            "SELECT to_jsonb(c) - 'created_xid' FROM access_review_campaigns c WHERE c.id=$1",
        )
        .bind(campaign)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = snapshot(pool.clone()).await;
    let path = format!("{SCHEDULES}/{schedule}");
    let (status, paused) = call(&f.s, &f.admin, "PATCH", &path, json!({"paused":true})).await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert!(paused["paused_at"].is_string() && paused["next_run_at"].is_null());
    // While paused, a due period does not run.
    sqlx::query("UPDATE access_review_schedules SET next_run_at=clock_timestamp()-interval '1 minute' WHERE id=$1")
        .bind(schedule)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$2,closed_at=now() WHERE id=$1")
        .bind(campaign)
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let before = {
        let mut b = before;
        let closed = snapshot(pool.clone()).await;
        b["closed_at"] = closed["closed_at"].clone();
        b["closed_by"] = closed["closed_by"].clone();
        b
    };
    assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    // Resuming starts at the next period: missed periods do not run.
    let (status, resumed) = call(&f.s, &f.admin, "PATCH", &path, json!({"paused":false})).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert!(resumed["paused_at"].is_null());
    assert!(next_is_next_month(&pool, schedule).await);
    assert!(run_due_fenced(&f.s, None).await.unwrap().is_empty());
    // Changing the calendar recomputes the next start from now.
    let (status, changed) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"name":"Lifecycle quarterly","cadence":"quarterly","start_day":31,"start_month":3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(
        (changed["cadence"].as_str(), changed["start_month"].as_i64()),
        (Some("quarterly"), Some(3))
    );
    let (status, _) = call(&f.s, &f.admin, "DELETE", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        snapshot(pool.clone()).await,
        before,
        "the campaign is untouched"
    );
    assert_eq!(
        call(&f.s, &f.admin, "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.s, &f.admin, "PATCH", &path, json!({"paused":true}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.s, &f.admin, "DELETE", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, list) = call(&f.s, &f.admin, "GET", SCHEDULES, Value::Null).await;
    assert_eq!(list["data"], json!([]));
    // The campaign still names its schedule.
    let (_, reviews) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/access-reviews",
        Value::Null,
    )
    .await;
    assert_eq!(reviews["data"][0]["schedule_id"], json!(schedule));
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE resource_type='access_review_schedule' AND resource_id=$1 AND actor_user_id=$2 ORDER BY created_at, id")
        .bind(schedule)
        .bind(f.admin.user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        actions,
        [
            "access_review_schedule.created",
            "access_review_schedule.paused",
            "access_review_schedule.resumed",
            "access_review_schedule.changed",
            "access_review_schedule.deleted"
        ]
    );
}

#[dbtest::test]
async fn schedules_are_managed_by_platform_admins_and_read_by_auditors(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Authority")).await;
    let path = format!("{SCHEDULES}/{schedule}");
    for (u, read) in [
        (&f.auditor, StatusCode::OK),
        (&f.owner, StatusCode::FORBIDDEN),
        (&f.member, StatusCode::FORBIDDEN),
    ] {
        assert_eq!(call(&f.s, u, "GET", SCHEDULES, Value::Null).await.0, read);
        assert_eq!(call(&f.s, u, "GET", &path, Value::Null).await.0, read);
        assert_eq!(
            call(&f.s, u, "POST", SCHEDULES, monthly(&f, "Refused"))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f.s, u, "PATCH", &path, json!({"paused":true}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f.s, u, "DELETE", &path, Value::Null).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let paused: Option<UtcInstant> =
        sqlx::query_scalar("SELECT paused_at FROM access_review_schedules WHERE id=$1")
            .bind(schedule)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(paused, None);
    assert_eq!(
        scalar(
            &pool,
            "SELECT count(*) FROM access_review_schedules WHERE $1::uuid IS NOT NULL",
            schedule
        )
        .await,
        1
    );
}

#[dbtest::test]
async fn a_schedule_is_validated_and_first_runs_at_the_next_period_start(pool: PgPool) {
    let f = fixture(&pool).await;
    let base = monthly(&f, "Valid");
    for (field, value) in [
        ("cadence", json!("weekly")),
        ("start_day", json!(0)),
        ("start_day", json!(32)),
        ("start_month", json!(2)),
        ("open_days", json!(0)),
        ("open_days", json!(91)),
        ("name", json!("x".repeat(101))),
        ("name", json!(" ")),
        ("subjects", json!([])),
        (
            "subjects",
            json!(["workspace_memberships", "workspace_memberships"]),
        ),
        ("workspace_ids", json!([f.personal])),
    ] {
        let mut body = base.clone();
        body[field] = value.clone();
        let (status, refused) = call(&f.s, &f.admin, "POST", SCHEDULES, body).await;
        assert!(
            status.is_client_error(),
            "{field}={value}: {status} {refused}"
        );
    }
    let mut quarterly = base.clone();
    quarterly["cadence"] = json!("quarterly");
    assert!(
        call(&f.s, &f.admin, "POST", SCHEDULES, quarterly.clone())
            .await
            .0
            .is_client_error(),
        "quarterly needs a start month"
    );
    quarterly["start_month"] = json!(1);
    assert_eq!(
        call(&f.s, &f.admin, "POST", SCHEDULES, quarterly).await.0,
        StatusCode::OK
    );
    // Created after this month's start day (the 1st): first runs next month.
    let schedule = create(&f, base.clone()).await;
    assert!(next_is_next_month(&pool, schedule).await);
    // A partial calendar change is refused; so is an empty change.
    let path = format!("{SCHEDULES}/{schedule}");
    assert!(
        call(&f.s, &f.admin, "PATCH", &path, json!({"start_day":5}))
            .await
            .0
            .is_client_error()
    );
    assert!(
        call(&f.s, &f.admin, "PATCH", &path, json!({}))
            .await
            .0
            .is_client_error()
    );
}

#[dbtest::test]
async fn resending_the_stored_calendar_keeps_a_due_run(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Resent")).await;
    due_since(&pool, schedule, 0).await;
    // An edit that wins the lock after the boundary, before the run: same
    // calendar, new name. The due period still runs.
    let (status, changed) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("{SCHEDULES}/{schedule}"),
        json!({"name":"Resent renamed","cadence":"monthly","start_day":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    assert!(
        matches!(ran[..], [(id, Outcome::Started(_))] if id == schedule),
        "{ran:?}"
    );
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE resource_id=$1 AND action='access_review_schedule.changed'")
        .bind(schedule)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(actions.len(), 1, "the name changed");
}

/// The database keeps one run per cursor position whoever writes: the guards
/// refuse a second run for a due period, a run for a future period, moving
/// the cursor back onto a period that ran, a change by a non-admin, and a
/// second incident for one skipped run, even after the first cleared.
#[dbtest::test]
async fn the_database_keeps_one_run_per_period(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Guarded")).await;
    due_since(&pool, schedule, 0).await;
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    assert!(matches!(ran[0].1, Outcome::Started(_)));
    let refused = |r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>| {
        r.err().and_then(|e| {
            e.as_database_error()
                .and_then(|d| d.code())
                .map(|c| c.into_owned())
        })
    };
    // The cursor back onto this month's period, which ran.
    assert_eq!(
        refused(
            sqlx::query(&format!(
                "UPDATE access_review_schedules SET next_run_at={THIS_MONTH} WHERE id=$1"
            ))
            .bind(schedule)
            .execute(&pool)
            .await
        )
        .as_deref(),
        Some("23514")
    );
    // A run for the current cursor when one ran, and one for a future period.
    let other = create(&f, monthly(&f, "Guarded other")).await;
    due_since(&pool, other, 0).await;
    let insert = |id: Uuid, period: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!("INSERT INTO access_review_schedule_runs(id,schedule_id,period_at,outcome,reason) VALUES($1,$2,{period},'skipped','empty')"))
                .bind(Uuid::now_v7())
                .bind(id)
                .execute(&pool)
                .await
        }
    };
    assert_eq!(
        refused(insert(other, "now()+interval '1 day'").await).as_deref(),
        Some("23514")
    );
    assert_eq!(refused(insert(other, THIS_MONTH).await), None);
    assert_eq!(
        refused(insert(other, "(date_trunc('month',now() AT TIME ZONE 'UTC') + interval '1 hour') AT TIME ZONE 'UTC'").await).as_deref(),
        Some("23514"),
        "one run per cursor position"
    );
    // A template change or a resume needs a current Platform Admin.
    assert_eq!(
        refused(sqlx::query("UPDATE access_review_schedules SET subjects='{platform_roles}',workspace_ids='{}',updated_by=$2 WHERE id=$1").bind(other).bind(f.member.user_id).execute(&pool).await).as_deref(),
        Some("23514")
    );
    // One incident per skipped run, for good.
    let run: Uuid =
        sqlx::query_scalar("SELECT id FROM access_review_schedule_runs WHERE schedule_id=$1")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        crate::alerts::fire_access_review_skipped(&mut tx, run, "empty")
            .await
            .unwrap()
    );
    assert_eq!(
        crate::alerts::resolve_access_review_skipped(&mut tx, other)
            .await
            .unwrap(),
        1
    );
    assert!(
        !crate::alerts::fire_access_review_skipped(&mut tx, run, "empty")
            .await
            .unwrap()
    );
    let incidents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alert_events WHERE subject_key='access_review_run:'||$1::text",
    )
    .bind(run)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(incidents, 1);
}

/// Twenty open campaigns can close within minutes: the run is retried each
/// tick for a day after its period starts, recording nothing, and only then
/// recorded as skipped.
#[dbtest::test]
async fn a_passing_condition_is_retried_for_a_day_before_the_run_is_skipped(pool: PgPool) {
    let f = fixture(&pool).await;
    for i in 0..20 {
        let (status, body) = call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/access-reviews",
            json!({"name":format!("Open {i}"),"subjects":["platform_roles"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // A start day of today (UTC), due since today's midnight.
    let today: i32 = sqlx::query_scalar("SELECT extract(day FROM now() AT TIME ZONE 'UTC')::int")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut body = monthly(&f, "Busy");
    body["start_day"] = json!(today);
    let schedule = create(&f, body).await;
    sqlx::query("UPDATE access_review_schedules SET next_run_at=date_trunc('day',now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' WHERE id=$1")
        .bind(schedule)
        .execute(&pool)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            run_due_fenced(&f.s, None).await.unwrap(),
            vec![(schedule, Outcome::Deferred(Skip::OpenLimit))]
        );
    }
    let recorded: i64 = scalar(&pool, "SELECT (SELECT count(*) FROM access_review_schedule_runs WHERE schedule_id=$1)+(SELECT count(*) FROM alert_events WHERE builtin='access_review_skipped')", schedule).await;
    assert_eq!(recorded, 0, "nothing recorded while retrying");
    // A campaign closes: the next tick starts the period's campaign.
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$1,closed_at=now() WHERE id=(SELECT id FROM access_review_campaigns WHERE schedule_id IS NULL AND closed_at IS NULL LIMIT 1)")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    assert!(
        matches!(ran[..], [(id, Outcome::Started(_))] if id == schedule),
        "{ran:?}"
    );
    // A period that began over a day ago with the limit still reached is skipped.
    let late = create(&f, monthly(&f, "Busy late")).await;
    due_since(&pool, late, 0).await;
    let open: i64 = scalar(&pool, "SELECT count(*) FROM access_review_campaigns WHERE closed_at IS NULL AND $1::uuid IS NOT NULL", late).await;
    assert_eq!(open, 20);
    let started_today: bool =
        sqlx::query_scalar("SELECT extract(day FROM now() AT TIME ZONE 'UTC') > 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    if started_today {
        assert_eq!(
            run_due_fenced(&f.s, None).await.unwrap(),
            vec![(late, Outcome::Skipped(Skip::OpenLimit))]
        );
    }
}

#[dbtest::test]
async fn a_later_started_run_clears_the_skipped_run_alert(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Clears")).await;
    due_since(&pool, schedule, 1).await;
    sqlx::query(&format!(
        "SELECT omg_access_review_start_scheduled($1,$2,$3,{THIS_MONTH} - interval '1 month')"
    ))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(schedule)
    .execute(&pool)
    .await
    .unwrap();
    due_since(&pool, schedule, 0).await;
    assert_eq!(
        run_due_fenced(&f.s, None).await.unwrap(),
        vec![(schedule, Outcome::Skipped(Skip::PreviousOpen))]
    );
    sqlx::query(
        "UPDATE access_review_campaigns SET closed_by=$2,closed_at=now() WHERE schedule_id=$1",
    )
    .bind(schedule)
    .bind(f.admin.user_id)
    .execute(&pool)
    .await
    .unwrap();
    // A new calendar (the 2nd), due for this month's 2nd: a new period.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("{SCHEDULES}/{schedule}"),
        json!({"cadence":"monthly","start_day":2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query(&format!(
        "UPDATE access_review_schedules SET next_run_at={THIS_MONTH} + interval '1 day' WHERE id=$1"
    ))
    .bind(schedule)
    .execute(&pool)
    .await
    .unwrap();
    let ran = run_due_fenced(&f.s, None).await.unwrap();
    assert!(matches!(ran[..], [(_, Outcome::Started(_))]), "{ran:?}");
    let (resolution, transitions): (Option<String>, Vec<String>) = sqlx::query_as("SELECT e.resolution,(SELECT array_agg(d.transition ORDER BY d.created_at) FROM alert_deliveries d WHERE d.event_id=e.id) FROM alert_events e WHERE e.builtin='access_review_skipped'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resolution.as_deref(), Some("cleared"));
    assert_eq!(transitions, ["fired", "resolved"]);
}

#[dbtest::test]
async fn schedules_are_limited_and_cover_shared_workspaces_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let first = create(&f, monthly(&f, "Limit 0")).await;
    let (status, refused) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("{SCHEDULES}/{first}"),
        json!({"workspace_ids":[f.personal]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    for i in 1..50 {
        create(&f, monthly(&f, &format!("Limit {i}"))).await;
    }
    let (status, refused) = call(&f.s, &f.admin, "POST", SCHEDULES, monthly(&f, "Limit 50")).await;
    assert_eq!(
        (status, refused["error"]["reason"].as_str()),
        (StatusCode::CONFLICT, Some("access_review_schedule_limit"))
    );
    // A deleted schedule frees its place.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("{SCHEDULES}/{first}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", SCHEDULES, monthly(&f, "Limit 50"))
            .await
            .0,
        StatusCode::OK
    );
}

#[dbtest::test]
async fn a_change_that_changes_nothing_writes_nothing(pool: PgPool) {
    let f = fixture(&pool).await;
    let schedule = create(&f, monthly(&f, "Unchanged")).await;
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM access_review_schedules s WHERE id=$1")
            .bind(schedule)
            .fetch_one(&pool)
            .await
            .unwrap();
    for body in [
        json!({"paused":false}),
        json!({"name":"Unchanged","cadence":"monthly","start_day":1}),
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PATCH",
                &format!("{SCHEDULES}/{schedule}"),
                body
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let after: Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM access_review_schedules s WHERE id=$1")
            .bind(schedule)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert_eq!(scalar(&pool, "SELECT count(*) FROM audit_events WHERE resource_id=$1 AND action<>'access_review_schedule.created'", schedule).await, 0);
}
