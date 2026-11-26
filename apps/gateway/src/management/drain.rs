//! Operator drains of deployments and provider connections (#49): Platform
//! Admins set, change and clear them; Platform Admins and Auditors read them.
//! What a drain does to routing is `routing::drain`; here it is one state per
//! resource (`drain_reason`, `drain_until`, `drained_by`, `drained_at`), set
//! and cleared together under the exclusive catalog lock, each change audited
//! with its reason.
//!
//! The reason is operator text: at most [`REASON_MAX_CHARS`] characters, with
//! no control, invisible or bidirectional character and no line separator
//! (so it cannot hide, reorder or break a line where it is shown), and it is
//! only ever returned as a JSON string. It is kept in the audit log (and so in
//! its export); it never reaches an inference client or an operational log
//! line.
use super::*;
use gateway_datetime_compat::UtcInstant;

/// The longest drain reason, in characters (the column's CHECK matches).
pub(crate) const REASON_MAX_CHARS: usize = 500;

/// A drain's `until` is at most this far ahead.
const UNTIL_MAX_DAYS: i32 = 366;

pub(super) const REASON_INVALID: &str = "A drain reason is 1 to 500 characters after trimming, with no control, invisible or bidirectional characters";
pub(super) const UNTIL_INVALID: &str =
    "A drain's until is in the future and at most 366 days ahead";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DrainInput {
    reason: String,
    #[serde(deserialize_with = "super::governance::nullable")]
    until: Option<UtcInstant>,
}

/// A drain reason as stored: trimmed, non-empty, bounded, with no control
/// character.
fn reason(raw: &str) -> Result<String, ApiError> {
    // The length the schema states (1..=500 characters as sent) is an
    // ordinary invalid request; what it cannot state (blank once trimmed, a
    // control character) has its own reason.
    let length = raw.chars().count();
    if length == 0 || length > REASON_MAX_CHARS {
        return Err(invalid());
    }
    let reason = raw.trim();
    if reason.is_empty() || reason.chars().any(hidden) {
        return Err(ApiError(StatusCode::BAD_REQUEST, REASON_INVALID));
    }
    Ok(reason.to_owned())
}

/// A character a reason may not hold: a control character, or one that is
/// invisible or reorders or breaks the text where it is shown (format
/// characters such as zero-width spaces and bidirectional controls, the line
/// and paragraph separators, blank fillers, variation selectors and tag
/// characters).
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                // Blank fillers that read as nothing, variation selectors and
                // tag characters.
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{FFA0}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// The drained resource's two kinds: its table and its audit resource type.
#[derive(Clone, Copy)]
enum Target {
    Deployment,
    Connection,
}
impl Target {
    fn table(self) -> &'static str {
        match self {
            Self::Deployment => "deployments",
            Self::Connection => "provider_connections",
        }
    }
    fn resource(self) -> &'static str {
        match self {
            Self::Deployment => "deployment",
            Self::Connection => "provider_connection",
        }
    }
}

/// The drain of the row aliased `a`, as the API returns it (`null` when there
/// is none): `state` is `active`, or `ended` once `until` has passed (an ended
/// drain no longer applies; it stays shown until cleared or set again).
/// `drained_by` names the governing user (an admin token's owner), with no
/// control characters.
fn drain_json(a: &str) -> String {
    format!(
        "CASE WHEN {a}.drained_at IS NULL THEN NULL ELSE jsonb_build_object(
            'reason',{a}.drain_reason,
            'until',{a}.drain_until,
            'state',CASE WHEN {a}.drain_until IS NOT NULL AND {a}.drain_until <= statement_timestamp() THEN 'ended' ELSE 'active' END,
            'drained_at',{a}.drained_at,
            'drained_by',jsonb_build_object('id',{a}.drained_by,'label',(SELECT coalesce(nullif(left(regexp_replace(u.display_name,'[[:cntrl:]]','','g'),160),''),nullif(left(regexp_replace(CASE WHEN u.cleaned_at IS NULL THEN u.email END,'[[:cntrl:]]','','g'),160),''),'User '||u.id::text) FROM users u WHERE u.id={a}.drained_by))) END"
    )
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/deployments/{deployment}/drain",
            get(deployment)
                .put(put_deployment)
                .delete(delete_deployment),
        )
        .route(
            "/api/v1/platform/providers/{id}/drain",
            get(connection)
                .put(put_connection)
                .delete(delete_connection),
        )
}

/// `GET /api/v1/platform/deployments/{deployment}/drain`: the route's own
/// drain and its connection's (either takes it out), and whether it is
/// drained now.
pub(super) async fn deployment(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let sql = format!(
        "SELECT jsonb_build_object('drain',{own},'connection',jsonb_build_object('id',p.id,'name',p.name,'drain',{conn}),'drained',{drained},'as_of',statement_timestamp()) FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1",
        own = drain_json("d"),
        conn = drain_json("p"),
        drained = crate::routing::drain::ROUTE_DRAINED,
    );
    let v: Value = sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `GET /api/v1/platform/providers/{id}/drain`: the connection's drain and
/// the routes it takes out while active (its enabled deployments).
pub(super) async fn connection(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let sql = format!(
        "SELECT jsonb_build_object('drain',{conn},'routes',(SELECT count(*) FROM deployments d WHERE d.provider_connection_id=p.id AND d.enabled),'as_of',statement_timestamp()) FROM provider_connections p WHERE p.id=$1",
        conn = drain_json("p"),
    );
    let v: Value = sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}

pub(super) async fn put_deployment(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(p): Json<DrainInput>,
) -> ApiResult {
    set(&s, &u, Target::Deployment, id, p).await
}
pub(super) async fn put_connection(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(p): Json<DrainInput>,
) -> ApiResult {
    set(&s, &u, Target::Connection, id, p).await
}
pub(super) async fn delete_deployment(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    clear(&s, &u, Target::Deployment, id).await
}
pub(super) async fn delete_connection(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    clear(&s, &u, Target::Connection, id).await
}

/// Set a drain, or change one: a new reason, `until`, who and when. Setting
/// over an ended drain is a new drain.
async fn set(
    s: &Store,
    u: &ManagementPrincipal,
    target: Target,
    id: Uuid,
    input: DrainInput,
) -> ApiResult {
    let reason = reason(&input.reason)?;
    let mut tx = resources::catalog_tx(s, u, true).await?;
    let table = target.table();
    // The previous drain, if any still applies; the row is locked for the change.
    let previous: Option<Option<bool>> = sqlx::query_scalar(&format!(
        "SELECT CASE WHEN drained_at IS NULL THEN NULL ELSE drain_until IS NULL OR drain_until > statement_timestamp() END FROM {table} WHERE id=$1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(previous) = previous else {
        return Err(missing());
    };
    if let Some(until) = input.until {
        let bounded: bool = sqlx::query_scalar(
            "SELECT $1 > statement_timestamp() AND $1 <= statement_timestamp() + make_interval(days => $2)",
        )
        .bind(until)
        .bind(UNTIL_MAX_DAYS)
        .fetch_one(&mut *tx)
        .await?;
        if !bounded {
            return Err(ApiError(StatusCode::BAD_REQUEST, UNTIL_INVALID));
        }
    }
    sqlx::query(&format!(
        "UPDATE {table} SET drain_reason=$2, drain_until=$3, drained_by=$4, drained_at=statement_timestamp() WHERE id=$1"
    ))
    .bind(id)
    .bind(&reason)
    .bind(input.until)
    .bind(u.user_id())
    .execute(&mut *tx)
    .await?;
    let action = if previous == Some(true) {
        "routing.drain_changed"
    } else {
        "routing.drain_set"
    };
    resources::audit(
        &mut tx,
        u,
        None,
        action,
        target.resource(),
        Some(id),
        json!({"drain_reason": reason, "drain_until": input.until}),
    )
    .await?;
    let routes = affected(&mut tx, target, id).await?;
    tx.commit().await?;
    forget(s, &routes);
    Ok(ok())
}

/// Clear a drain (active or ended). Clearing none changes nothing.
async fn clear(s: &Store, u: &ManagementPrincipal, target: Target, id: Uuid) -> ApiResult {
    let mut tx = resources::catalog_tx(s, u, true).await?;
    let table = target.table();
    let previous: Option<Option<String>> = sqlx::query_scalar(&format!(
        "SELECT drain_reason FROM {table} WHERE id=$1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(previous) = previous else {
        return Err(missing());
    };
    if let Some(reason) = previous {
        sqlx::query(&format!(
            "UPDATE {table} SET drain_reason=NULL, drain_until=NULL, drained_by=NULL, drained_at=NULL WHERE id=$1"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;
        resources::audit(
            &mut tx,
            u,
            None,
            "routing.drain_cleared",
            target.resource(),
            Some(id),
            json!({"drain_reason": reason}),
        )
        .await?;
    }
    let routes = affected(&mut tx, target, id).await?;
    tx.commit().await?;
    forget(s, &routes);
    Ok(ok())
}

/// The routes a drain change of `id` applies to.
async fn affected(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: Target,
    id: Uuid,
) -> Result<Vec<Uuid>, ApiError> {
    Ok(match target {
        Target::Deployment => vec![id],
        Target::Connection => {
            sqlx::query_scalar("SELECT id FROM deployments WHERE provider_connection_id=$1")
                .bind(id)
                .fetch_all(&mut **tx)
                .await?
        }
    })
}

/// This replica sees the change at once; another within `HEALTH_TTL`, and
/// admission everywhere at once (`routing::drain`).
fn forget(s: &Store, routes: &[Uuid]) {
    for route in routes {
        s.caches.forget_health(*route);
    }
}

/// The audit fields of a drain change: its reason (bounded operator text,
/// checked again here) and `until`.
pub(super) fn audit_field(action: &str, key: &str, value: &Value) -> bool {
    action.starts_with("routing.drain_")
        && match key {
            "drain_reason" => value.as_str().is_some_and(|r| {
                !r.is_empty() && r.chars().count() <= REASON_MAX_CHARS && !r.chars().any(hidden)
            }),
            "drain_until" => {
                value.is_null()
                    || value.as_str().is_some_and(|t| t.len() <= 40)
                        && serde_json::from_value::<UtcInstant>(value.clone()).is_ok()
            }
            _ => false,
        }
}
