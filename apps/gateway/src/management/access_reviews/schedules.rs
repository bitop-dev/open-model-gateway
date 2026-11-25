//! Access review schedules (#481). A Platform Admin defines a campaign
//! template (name, subjects, workspace filter, days open) and a fixed cadence
//! ([`calendar`]); the lifecycle job starts each due run once across replicas
//! ([`run_due_fenced`]). A run starts an ordinary campaign, recorded as
//! started by the schedule and naming the admin who created the schedule, or
//! is recorded as skipped (and raises one Platform Admin alert). The
//! scheduler acts as the system, never with the creator's authority: it keeps
//! running if that account is later demoted or disabled. Pausing, resuming,
//! changing and deleting a schedule never touch the campaigns it started.
//! Platform Admins manage schedules; Platform Auditors read them.
use super::*;
use gateway_datetime_compat::UtcInstant;

mod calendar;
pub(crate) use calendar::{Cadence, Calendar};

/// Schedules that are not deleted, at once.
const MAX_SCHEDULES: i64 = 50;
/// Schedule names leave room for the period date in each campaign's name.
const MAX_NAME_CHARS: usize = 100;
/// Due schedules the lifecycle job runs per tick (each in its own
/// transaction); the rest run on the next tick.
const RUNS_PER_TICK: i64 = 20;
/// No new run starts after this much of a tick: the lifecycle job's deadline
/// (30 s) is never spent on schedules alone, and the rest run next tick.
const TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// Recent runs in a schedule's detail.
const RECENT_RUNS: i64 = 20;

const RUN_JSON: &str = "jsonb_build_object('id',r.id,'period_at',r.period_at,'ran_at',r.ran_at,'outcome',r.outcome,'reason',r.reason,'campaign_id',r.campaign_id)";

fn schedule_json() -> String {
    format!(
        "jsonb_build_object('id',s.id,'name',s.name,'subjects',to_jsonb(s.subjects),'workspace_ids',to_jsonb(s.workspace_ids),'open_days',s.open_days,'cadence',s.cadence,'start_day',s.start_day,'start_month',s.start_month,'created_by',s.created_by,'created_at',s.created_at,'updated_at',s.updated_at,'paused_at',s.paused_at,'next_run_at',CASE WHEN s.paused_at IS NULL THEN s.next_run_at END,'last_run',(SELECT {RUN_JSON} FROM access_review_schedule_runs r WHERE r.schedule_id=s.id ORDER BY r.period_at DESC LIMIT 1),'last_campaign',(SELECT jsonb_build_object('id',c.id,'name',c.name,'created_at',c.created_at,'due_at',c.due_at,'closed_at',c.closed_at) FROM access_review_campaigns c WHERE c.schedule_id=s.id ORDER BY c.created_at DESC, c.id DESC LIMIT 1))"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::management) struct NewSchedule {
    name: String,
    subjects: Vec<String>,
    #[serde(default)]
    workspace_ids: Vec<Uuid>,
    #[serde(default)]
    open_days: Option<i32>,
    cadence: String,
    start_day: i32,
    #[serde(default)]
    start_month: Option<i32>,
}

/// Every field is optional. `cadence` replaces the whole calendar: with it,
/// `start_day` is required and `start_month` is read as given (absent is
/// none); without it, neither may be sent. `paused` pauses or resumes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::management) struct ScheduleChange {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    subjects: Option<Vec<String>>,
    #[serde(default)]
    workspace_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    open_days: Option<i32>,
    #[serde(default)]
    cadence: Option<String>,
    #[serde(default)]
    start_day: Option<i32>,
    #[serde(default)]
    start_month: Option<i32>,
    #[serde(default)]
    paused: Option<bool>,
}

fn calendar_of(cadence: &str, start_day: i32, start_month: Option<i32>) -> Option<Calendar> {
    let day = u8::try_from(start_day).ok()?;
    let month = match start_month {
        Some(m) => Some(u8::try_from(m).ok()?),
        None => None,
    };
    Calendar::new(Cadence::parse(cadence)?, day, month)
}

/// The template is within the bounds a campaign has: a name (shorter than a
/// campaign's), known distinct subjects, 1 to 90 days open and at most 50
/// distinct workspaces (only for subjects that have workspaces).
fn template_valid(name: &str, subjects: &[String], workspace_ids: &[Uuid], open_days: i32) -> bool {
    let distinct: std::collections::BTreeSet<&str> = subjects.iter().map(String::as_str).collect();
    let filter: std::collections::BTreeSet<Uuid> = workspace_ids.iter().copied().collect();
    let body = NewCampaign {
        name: name.to_owned(),
        subjects: subjects.to_vec(),
        workspace_ids: workspace_ids.to_vec(),
        due_in_days: Some(open_days),
    };
    chars_at_most(name, MAX_NAME_CHARS) && campaign_body_valid(&body, open_days, &distinct, &filter)
}

fn distinct_subjects(subjects: &[String]) -> Vec<String> {
    let set: std::collections::BTreeSet<&str> = subjects.iter().map(String::as_str).collect();
    set.into_iter().map(str::to_owned).collect()
}

async fn now(tx: &mut Transaction<'_, Postgres>) -> Result<UtcInstant, sqlx::Error> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await
}

/// Every calendar has a period start within 12 months, so this is a
/// corrupt row, never a request error.
fn calendar_exhausted() -> ApiError {
    sqlx::Error::Protocol("access review schedule has no next period".into()).into()
}

async fn one(tx: &mut Transaction<'_, Postgres>, id: Uuid, runs: bool) -> ApiResult {
    let json = schedule_json();
    let runs_json = if runs {
        format!(
            " || jsonb_build_object('runs',coalesce((SELECT jsonb_agg(x.v ORDER BY x.period_at DESC) FROM (SELECT {RUN_JSON} v, r.period_at FROM access_review_schedule_runs r WHERE r.schedule_id=s.id ORDER BY r.period_at DESC LIMIT {RECENT_RUNS}) x),'[]'::jsonb))"
        )
    } else {
        String::new()
    };
    let v: Value = sqlx::query_scalar(&format!(
        "SELECT {json}{runs_json} FROM access_review_schedules s WHERE s.id=$1 AND s.deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)?;
    Ok(Json(v))
}

/// `GET /api/v1/platform/access-review-schedules`: schedules that are not
/// deleted, oldest first, each with its next start, last run and last
/// campaign.
pub(in crate::management) async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let json = schedule_json();
    let data: Vec<Value> = sqlx::query_scalar(&format!(
        "SELECT {json} FROM access_review_schedules s WHERE s.deleted_at IS NULL ORDER BY s.created_at, s.id LIMIT $1 OFFSET $2"
    ))
    .bind(l)
    .bind(o)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

/// `GET /api/v1/platform/access-review-schedules/{id}`: one schedule with its
/// 20 most recent runs.
pub(in crate::management) async fn detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let result = one(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(result)
}

/// `POST /api/v1/platform/access-review-schedules`. The first run is the
/// first period start after now: a schedule created after this period's
/// start day first runs next period.
pub(in crate::management) async fn create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<NewSchedule>,
) -> Result<Json<Value>, ManagementError> {
    let open_days = b.open_days.unwrap_or(DEFAULT_DUE_DAYS);
    let calendar = calendar_of(&b.cadence, b.start_day, b.start_month).ok_or_else(invalid)?;
    if !template_valid(&b.name, &b.subjects, &b.workspace_ids, open_days) {
        return Err(invalid().into());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    if !b.workspace_ids.is_empty() {
        check_workspace_filter(&mut tx, &b.workspace_ids, b.workspace_ids.len()).await?;
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM access_review_schedules WHERE deleted_at IS NULL")
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_SCHEDULES {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_schedule_limit",
            "There are already 50 access review schedules; delete one first".into(),
            json!({}),
        ));
    }
    let next = calendar
        .first_after(now(&mut tx).await?)
        .ok_or_else(calendar_exhausted)?;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO access_review_schedules(id,name,subjects,workspace_ids,open_days,cadence,start_day,start_month,created_by,next_run_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(id)
        .bind(&b.name)
        .bind(distinct_subjects(&b.subjects))
        .bind(&b.workspace_ids)
        .bind(open_days)
        .bind(&b.cadence)
        .bind(b.start_day as i16)
        .bind(b.start_month.map(|m| m as i16))
        .bind(u.user_id)
        .bind(next)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "access_review_schedule.created",
        "access_review_schedule",
        Some(id),
        json!({"mode":b.cadence}),
    )
    .await?;
    let result = one(&mut tx, id, false).await?;
    tx.commit().await?;
    Ok(result)
}

#[derive(sqlx::FromRow)]
struct Current {
    name: String,
    subjects: Vec<String>,
    workspace_ids: Vec<Uuid>,
    open_days: i32,
    cadence: String,
    start_day: i16,
    start_month: Option<i16>,
    paused: bool,
}

/// `PATCH /api/v1/platform/access-review-schedules/{id}`: changes the
/// template or calendar, pauses or resumes. A changed calendar, and
/// resuming, set the next run to the first period start after now: periods
/// missed while paused do not run. Campaigns already started are untouched.
pub(in crate::management) async fn change(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<ScheduleChange>,
) -> Result<Json<Value>, ManagementError> {
    let new_calendar = match (&b.cadence, b.start_day, b.start_month) {
        (Some(c), Some(day), month) => Some(calendar_of(c, day, month).ok_or_else(invalid)?),
        (None, None, None) => None,
        _ => return Err(invalid().into()),
    };
    let template_changed = b.name.is_some()
        || b.subjects.is_some()
        || b.workspace_ids.is_some()
        || b.open_days.is_some();
    if !template_changed && new_calendar.is_none() && b.paused.is_none() {
        return Err(invalid().into());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    let current: Current = sqlx::query_as("SELECT name,subjects,workspace_ids,open_days,cadence,start_day,start_month,paused_at IS NOT NULL AS paused FROM access_review_schedules WHERE id=$1 AND deleted_at IS NULL FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let name = b.name.clone().unwrap_or_else(|| current.name.clone());
    let subjects = b
        .subjects
        .clone()
        .unwrap_or_else(|| current.subjects.clone());
    let workspace_ids = b
        .workspace_ids
        .clone()
        .unwrap_or_else(|| current.workspace_ids.clone());
    let open_days = b.open_days.unwrap_or(current.open_days);
    if !template_valid(&name, &subjects, &workspace_ids, open_days) {
        return Err(invalid().into());
    }
    if b.workspace_ids.is_some() && !workspace_ids.is_empty() {
        check_workspace_filter(&mut tx, &workspace_ids, workspace_ids.len()).await?;
    }
    let (cadence, start_day, start_month) = match (&b.cadence, b.start_day) {
        (Some(c), Some(day)) => (c.clone(), day as i16, b.start_month.map(|m| m as i16)),
        _ => (
            current.cadence.clone(),
            current.start_day,
            current.start_month,
        ),
    };
    let pause = b.paused == Some(true) && !current.paused;
    let resume = b.paused == Some(false) && current.paused;
    let calendar = calendar_of(&cadence, start_day.into(), start_month.map(Into::into))
        .ok_or_else(calendar_exhausted)?;
    // Resending the stored calendar is no change: the cursor stays, so a
    // period that is due but not yet run still runs.
    let calendar_changed = (cadence.as_str(), start_day, start_month)
        != (
            current.cadence.as_str(),
            current.start_day,
            current.start_month,
        );
    let template_differs = (
        &name,
        &distinct_subjects(&subjects),
        &workspace_ids,
        open_days,
    ) != (
        &current.name,
        &current.subjects,
        &current.workspace_ids,
        current.open_days,
    );
    if !template_differs && !calendar_changed && !pause && !resume {
        // Nothing changes: no write and no audit event.
        let result = one(&mut tx, id, false).await?;
        tx.commit().await?;
        return Ok(result);
    }
    let next = if calendar_changed || resume {
        Some(
            calendar
                .first_after(now(&mut tx).await?)
                .ok_or_else(calendar_exhausted)?,
        )
    } else {
        None
    };
    sqlx::query("UPDATE access_review_schedules SET name=$2,subjects=$3,workspace_ids=$4,open_days=$5,cadence=$6,start_day=$7,start_month=$8,next_run_at=coalesce($9,next_run_at),paused_at=CASE WHEN $10 THEN clock_timestamp() WHEN $11 THEN NULL ELSE paused_at END,paused_by=CASE WHEN $10 THEN $12 WHEN $11 THEN NULL ELSE paused_by END,updated_by=$12,updated_at=clock_timestamp() WHERE id=$1")
        .bind(id)
        .bind(&name)
        .bind(distinct_subjects(&subjects))
        .bind(&workspace_ids)
        .bind(open_days)
        .bind(&cadence)
        .bind(start_day)
        .bind(start_month)
        .bind(next)
        .bind(pause)
        .bind(resume)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    if template_differs || calendar_changed {
        audit(
            &mut tx,
            &u,
            None,
            "access_review_schedule.changed",
            "access_review_schedule",
            Some(id),
            json!({"mode":cadence}),
        )
        .await?;
    }
    for (happened, action) in [
        (pause, "access_review_schedule.paused"),
        (resume, "access_review_schedule.resumed"),
    ] {
        if happened {
            audit(
                &mut tx,
                &u,
                None,
                action,
                "access_review_schedule",
                Some(id),
                json!({}),
            )
            .await?;
        }
    }
    let result = one(&mut tx, id, false).await?;
    tx.commit().await?;
    Ok(result)
}

/// `DELETE /api/v1/platform/access-review-schedules/{id}`: no further runs.
/// The schedule's runs and campaigns stay; its open skipped-run alerts clear.
pub(in crate::management) async fn delete(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ManagementError> {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    let deleted = sqlx::query("UPDATE access_review_schedules SET deleted_at=clock_timestamp(),deleted_by=$2,updated_at=clock_timestamp() WHERE id=$1 AND deleted_at IS NULL")
        .bind(id)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(missing().into());
    }
    crate::alerts::resolve_access_review_skipped(&mut tx, id).await?;
    audit(
        &mut tx,
        &u,
        None,
        "access_review_schedule.deleted",
        "access_review_schedule",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/access-review-schedules",
            get(list).post(create),
        )
        .route(
            "/api/v1/platform/access-review-schedules/{id}",
            get(detail).patch(change).delete(delete),
        )
}

/// Why a run did not start a campaign (the run's `reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Skip {
    /// The schedule's previous campaign is still open.
    PreviousOpen,
    /// 20 campaigns are already open.
    OpenLimit,
    /// No current access matches the template.
    Empty,
    /// More than 5,000 items.
    TooLarge,
    /// Access changed while the snapshot was taken.
    Changed,
}
impl Skip {
    /// A condition that can pass within minutes (a campaign closes, or a
    /// revocation finishes): the run is retried each tick for a day after its
    /// period starts before it is recorded as skipped, so a race never costs
    /// a whole period.
    const fn transient(self) -> bool {
        matches!(self, Self::OpenLimit | Self::Changed)
    }
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PreviousOpen => "previous_open",
            Self::OpenLimit => "open_limit",
            Self::Empty => "empty",
            Self::TooLarge => "too_large",
            Self::Changed => "changed",
        }
    }
}

/// What one due schedule's run did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Started(Uuid),
    Skipped(Skip),
    /// Nothing recorded: the schedule stays due and is retried next tick.
    Deferred(Skip),
}

#[derive(sqlx::FromRow)]
struct Due {
    subjects: Vec<String>,
    workspace_ids: Vec<Uuid>,
    cadence: String,
    start_day: i16,
    start_month: Option<i16>,
    created_by: Uuid,
    next_run_at: UtcInstant,
    now: UtcInstant,
}

/// Runs every due schedule (up to [`RUNS_PER_TICK`]) under the lifecycle
/// lease: each in its own fenced transaction. Returns what each run did.
pub(crate) async fn run_due_fenced(
    store: &Store,
    fence: Option<&crate::leases::Fence>,
) -> Result<Vec<(Uuid, Outcome)>, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM access_review_schedules WHERE paused_at IS NULL AND deleted_at IS NULL AND next_run_at<=clock_timestamp() ORDER BY next_run_at, id LIMIT $1")
        .bind(RUNS_PER_TICK)
        .fetch_all(&store.pool)
        .await?;
    // One schedule's failure (its transaction rolled back, so it stays due
    // for the next tick) never holds back the others; the first error is
    // returned after every due schedule was tried. A lost lease stops at once.
    let mut done = Vec::new();
    let mut first_error = None;
    let started = std::time::Instant::now();
    for id in due {
        if started.elapsed() >= TICK_BUDGET {
            break;
        }
        match run_one(store, fence, id).await {
            Ok(Some(outcome)) => done.push((id, outcome)),
            Ok(None) => {}
            Err(e) if crate::leases::is_fenced(&e) => return Err(e),
            Err(e) => {
                tracing::warn!(schedule_id=%id, "access review schedule run failed; it stays due and is retried next tick");
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(done),
    }
}

/// One schedule's run, exactly once per period: the run takes the
/// installation lock and the schedule row (skipping a row another run holds),
/// rechecks that it is still due, and in the same transaction records the
/// run (unique per schedule and period) and moves `next_run_at` to the first
/// period start after now. A run that dies before committing leaves the
/// schedule due, so the next tick runs it (late, once). After downtime the
/// run is for the latest period start; earlier missed periods are not
/// backfilled.
async fn run_one(
    store: &Store,
    fence: Option<&crate::leases::Fence>,
    id: Uuid,
) -> Result<Option<Outcome>, sqlx::Error> {
    let mut tx = resources::installation_tx_fenced(store, fence).await?;
    let Some(due) = sqlx::query_as::<_, Due>("SELECT s.subjects,s.workspace_ids,s.cadence,s.start_day,s.start_month,s.created_by,s.next_run_at,c.now FROM access_review_schedules s, (SELECT clock_timestamp() AS now) c WHERE s.id=$1 AND s.paused_at IS NULL AND s.deleted_at IS NULL AND s.next_run_at<=c.now FOR UPDATE OF s SKIP LOCKED")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    let calendar = calendar_of(
        &due.cadence,
        due.start_day.into(),
        due.start_month.map(Into::into),
    )
    .ok_or_else(|| sqlx::Error::Protocol("invalid access review schedule calendar".into()))?;
    // next_run_at is always a period start, so the latest one is at or after it.
    let period = calendar
        .latest_at_or_before(due.now)
        .filter(|p| *p >= due.next_run_at)
        .unwrap_or(due.next_run_at);
    let next = calendar
        .first_after(due.now)
        .ok_or_else(|| sqlx::Error::Protocol("access review schedule has no next period".into()))?;
    let outcome = start_or_skip(&mut tx, id, period, &due).await?;
    if let Outcome::Skipped(reason) = outcome
        && reason.transient()
        && due.now < period + gateway_datetime_compat::SignedDuration::days(1)
    {
        tx.rollback().await?;
        return Ok(Some(Outcome::Deferred(reason)));
    }
    match outcome {
        Outcome::Started(campaign) => {
            crate::alerts::resolve_access_review_skipped(&mut tx, id).await?;
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM access_review_items WHERE campaign_id=$1")
                    .bind(campaign)
                    .fetch_one(&mut *tx)
                    .await?;
            sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata) VALUES($1,'access_review.started_by_schedule','access_review',$2,jsonb_build_object('schedule_id',$3::text,'created_by',$4::text,'count',$5::bigint))")
                .bind(Uuid::now_v7())
                .bind(campaign)
                .bind(id)
                .bind(due.created_by)
                .bind(count)
                .execute(&mut *tx)
                .await?;
        }
        Outcome::Deferred(_) => {
            tx.rollback().await?;
            return Ok(Some(outcome));
        }
        Outcome::Skipped(reason) => {
            let run = Uuid::now_v7();
            sqlx::query("INSERT INTO access_review_schedule_runs(id,schedule_id,period_at,outcome,reason) VALUES($1,$2,$3,'skipped',$4)")
                .bind(run)
                .bind(id)
                .bind(period)
                .bind(reason.as_str())
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata) VALUES($1,'access_review_schedule.run_skipped','access_review_schedule',$2,jsonb_build_object('reason',$3::text,'created_by',$4::text))")
                .bind(Uuid::now_v7())
                .bind(id)
                .bind(reason.as_str())
                .bind(due.created_by)
                .execute(&mut *tx)
                .await?;
            crate::alerts::fire_access_review_skipped(&mut tx, run, reason.as_str()).await?;
        }
    }
    sqlx::query("UPDATE access_review_schedules SET next_run_at=$2 WHERE id=$1")
        .bind(id)
        .bind(next)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(outcome))
}

/// Starts the period's campaign (the definer function records the started
/// run) and snapshots its items in a savepoint, or says why the run is
/// skipped. A skipped run leaves no campaign.
async fn start_or_skip(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    period: UtcInstant,
    due: &Due,
) -> Result<Outcome, sqlx::Error> {
    let (previous_open, open): (bool, i64) = sqlx::query_as("SELECT EXISTS(SELECT 1 FROM access_review_campaigns WHERE schedule_id=$1 AND closed_at IS NULL), (SELECT count(*) FROM access_review_campaigns WHERE closed_at IS NULL)")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    if previous_open {
        return Ok(Outcome::Skipped(Skip::PreviousOpen));
    }
    if open >= MAX_OPEN {
        return Ok(Outcome::Skipped(Skip::OpenLimit));
    }
    let subjects: std::collections::BTreeSet<&str> =
        due.subjects.iter().map(String::as_str).collect();
    let campaign = Uuid::now_v7();
    // A nested transaction on the same connection is a savepoint.
    let mut savepoint = sqlx::Connection::begin(&mut **tx).await?;
    sqlx::query("SELECT omg_access_review_start_scheduled($1,$2,$3,$4)")
        .bind(campaign)
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(period)
        .execute(&mut *savepoint)
        .await?;
    let skip = match snapshot_rows(&mut savepoint, campaign, &subjects, &due.workspace_ids).await {
        Ok(n) if n > MAX_ITEMS => Some(Skip::TooLarge),
        Ok(0) => Some(Skip::Empty),
        Ok(_) => None,
        Err(e) if snapshot_changed(&e) => Some(Skip::Changed),
        Err(e) => return Err(e),
    };
    match skip {
        Some(reason) => {
            savepoint.rollback().await?;
            Ok(Outcome::Skipped(reason))
        }
        None => {
            savepoint.commit().await?;
            Ok(Outcome::Started(campaign))
        }
    }
}
