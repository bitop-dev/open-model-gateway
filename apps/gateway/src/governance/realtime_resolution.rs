//! Operator resolution of a finished realtime session's unknown responses
//! (#804). Each unknown response is settled from evidence, valued with the
//! session's pinned price exactly as `response.done` would have been; the
//! session settles once its last unknown response is resolved, with one
//! `reconciliation` ledger entry. The refusals mirror request resolution
//! ([`super::resolve_usage`]).
use super::realtime::{Session, Totals, pinned, session, totals, value_requests, value_response};
use super::*;
use crate::inference::realtime::{RealtimeUsage, ResponseBound};

/// What a resolution of one realtime response did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The response settled; others of the session are still unknown.
    Response,
    /// The response settled and so did its session (the last unknown one).
    Session,
    /// The same evidence and counters were already recorded: nothing written.
    Replay,
}

/// Resolve unknown response `sequence` of a finished realtime session from an
/// operator's evidence. Refusals:
/// - `InvalidRequest`: not a realtime session of `workspace`, no such
///   response, invalid counters or evidence, counters (or uncached input)
///   below what the response recorded, a value below its recorded floor, a
///   response already settled with other evidence, or an actor who is no
///   longer a live Platform Admin of a shared workspace (or of their own
///   personal one);
/// - `Busy`: the session or the response is still open (nothing is resolved
///   while the session can still settle it);
/// - `Configuration`: no pinned v3 price, or a counter whose rate is missing
///   (missing rates stay unknown, never zero);
/// - `Unauthenticated`: the actor's admin token was revoked meanwhile.
///
/// Lock order (`governance::locks`): the shared catalog lock and the actor's
/// workspace and user authority, then the session's reservation row, then
/// the response row, then the session's totals rows. Two resolutions of one
/// session therefore serialize on its reservation row: the second sees what
/// the first wrote, and exactly one settles the session.
pub async fn resolve_response(
    store: &Store,
    workspace: Uuid,
    execution: Uuid,
    sequence: i32,
    usage: RealtimeUsage,
    evidence: &str,
    actor: impl Into<super::ReconcilingActor>,
) -> Result<Resolved, InferenceError> {
    let actor = actor.into();
    if !valid_evidence(evidence) {
        return Err(InferenceError::InvalidRequest);
    }
    let wanted = stored(usage)?;
    let request = Request {
        workspace,
        execution,
        sequence,
        usage,
        wanted,
        evidence,
        actor,
    };
    locks::retry_deadlocks!(
        "reconciliation",
        resolve_response_once(store, &request).await
    )
}

struct Request<'a> {
    workspace: Uuid,
    execution: Uuid,
    sequence: i32,
    usage: RealtimeUsage,
    /// The six counters as stored, in column order.
    wanted: [i64; 6],
    evidence: &'a str,
    actor: super::ReconcilingActor,
}

/// The operator's evidence reference: 1-200 characters, not blank, no
/// control characters (the column CHECK's rule).
fn valid_evidence(evidence: &str) -> bool {
    !evidence.trim().is_empty()
        && evidence.chars().count() <= 200
        && !evidence.chars().any(char::is_control)
}

/// Valid usage as the six stored counters, in column order.
fn stored(usage: RealtimeUsage) -> Result<[i64; 6], InferenceError> {
    if !usage.valid() {
        return Err(InferenceError::InvalidRequest);
    }
    let mut out = [0; 6];
    for (slot, n) in out.iter_mut().zip(counters(usage)) {
        *slot = i64::try_from(n).map_err(|_| InferenceError::InvalidRequest)?;
    }
    Ok(out)
}

fn counters(u: RealtimeUsage) -> [u64; 6] {
    [
        u.input_text_tokens,
        u.cached_text_tokens,
        u.input_audio_tokens,
        u.cached_audio_tokens,
        u.output_text_tokens,
        u.output_audio_tokens,
    ]
}

/// One response row as recorded.
#[derive(sqlx::FromRow)]
struct ResponseRow {
    state: String,
    input_text_tokens: Option<i64>,
    cached_text_tokens: Option<i64>,
    input_audio_tokens: Option<i64>,
    cached_audio_tokens: Option<i64>,
    output_text_tokens: Option<i64>,
    output_audio_tokens: Option<i64>,
    evidence: Option<String>,
    floor_microusd: Option<i64>,
}
impl ResponseRow {
    fn counters(&self) -> [Option<i64>; 6] {
        [
            self.input_text_tokens,
            self.cached_text_tokens,
            self.input_audio_tokens,
            self.cached_audio_tokens,
            self.output_text_tokens,
            self.output_audio_tokens,
        ]
    }
    /// Evidence never lowers what the response recorded: no counter, and no
    /// uncached input (input minus cached) where both were recorded.
    fn lowered_by(&self, wanted: &[i64; 6]) -> bool {
        let recorded = self.counters();
        let uncached = |i: usize| {
            recorded[i]
                .zip(recorded[i + 1])
                .is_some_and(|(input, cached)| wanted[i] - wanted[i + 1] < input - cached)
        };
        recorded
            .iter()
            .zip(wanted)
            .any(|(old, new)| old.is_some_and(|old| *new < old))
            || uncached(0)
            || uncached(2)
    }
}

/// The response row, locked after the session's reservation row (canonical
/// order).
async fn load_response(
    tx: &mut Tx<'_>,
    execution: Uuid,
    sequence: i32,
) -> Result<Option<ResponseRow>, InferenceError> {
    sqlx::query_as("SELECT state,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,evidence,floor_microusd FROM realtime_responses WHERE execution_id=$1 AND sequence=$2 FOR UPDATE")
        .bind(execution)
        .bind(sequence)
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)
}

/// What a finished session's hold holds beyond its response rows.
#[derive(Debug, PartialEq, Eq)]
enum Unrowed {
    /// Nothing: the rows account for the whole hold.
    Nothing,
    /// One reserved window without a row, holding this much (0 for a window
    /// the price could not bound).
    Window(i64),
    /// The hold cannot be explained by its rows.
    Unattributed,
}

/// `finish` gives every forwarded request a response row, so a session it
/// closed holds nothing beyond them. One closed by lease expiry (its process
/// stopped) may still hold a reserved window with no row: a request
/// forwarded before the stop, which the provider may have run. A session
/// reserves one window at a time (`RealtimeSession::reserve_window`), so at
/// most one is unrowed, and its hold is what the hold exceeds the rows by.
/// What each row put into the hold: a window the price could not bound,
/// nothing (it only marked the session unbounded); a row settled by
/// `response.done`, its actual; any other (unknown, or resolved, which never
/// changes the hold), its window or floor, whichever is greater. Windows and
/// floors never change (the response guard), so this is the same before and
/// after every resolution. A session marked unbounded may hold an unrowed
/// window the price could not bound, which adds nothing to the hold: it is
/// never taken as complete.
async fn unrowed(
    tx: &mut Tx<'_>,
    execution: Uuid,
    expired: bool,
    s: &Session,
) -> Result<Unrowed, InferenceError> {
    if !expired {
        return Ok(Unrowed::Nothing);
    }
    let rows: i64 = sqlx::query_scalar("SELECT coalesce(sum(CASE WHEN window_hold_microusd IS NULL THEN 0 WHEN state='settled' AND evidence IS NULL THEN actual_microusd ELSE greatest(window_hold_microusd,coalesce(floor_microusd,0)) END),0)::bigint FROM realtime_responses WHERE execution_id=$1")
        .bind(execution)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(match s.held_microusd.and_then(|h| h.checked_sub(rows)) {
        Some(0) if !s.unbounded_cost => Unrowed::Nothing,
        Some(window) if window >= 0 => Unrowed::Window(window),
        _ => Unrowed::Unattributed,
    })
}

async fn resolve_response_once(store: &Store, q: &Request<'_>) -> Result<Resolved, InferenceError> {
    let mut tx = super::reconcile::operator_tx(store, q.workspace, q.actor).await?;
    let r = reservation(&mut tx, q.execution).await?;
    if r.workspace_id != q.workspace || r.workload_kind != WorkloadKind::Realtime.as_str() {
        return Err(InferenceError::InvalidRequest);
    }
    super::reconcile::operator_authorized(&mut tx, q.actor, q.workspace).await?;
    let s = session(&mut tx, q.execution).await?;
    let (open, expired): (bool, bool) = sqlx::query_as(
        "SELECT state='started',error_code IS NOT DISTINCT FROM 'lease_expired' FROM inference_executions WHERE id=$1 AND started_at=$2",
    )
    .bind(q.execution)
    .bind(r.admitted_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    if r.state == "pending" || open {
        return Err(InferenceError::Busy);
    }
    let row = load_response(&mut tx, q.execution, q.sequence)
        .await?
        .ok_or(InferenceError::InvalidRequest)?;
    match row.state.as_str() {
        // Only the identical resolution again is success; nothing is written.
        "settled" => {
            return if row.evidence.as_deref() == Some(q.evidence)
                && row.counters() == q.wanted.map(Some)
            {
                Ok(Resolved::Replay)
            } else {
                Err(InferenceError::InvalidRequest)
            };
        }
        "pending" => return Err(InferenceError::Busy),
        _ => {}
    }
    if row.lowered_by(&q.wanted) {
        return Err(InferenceError::InvalidRequest);
    }
    let price = pinned(&mut tx, &s)
        .await?
        .ok_or(InferenceError::Configuration)?;
    if price.pricing_version != 3 {
        return Err(InferenceError::Configuration);
    }
    // Every input at the context-window cap: the bound only decides whether
    // the window was exceeded, which no longer matters once usage is known.
    let bound = ResponseBound::unknown(u32::try_from(q.usage.output_tokens()).unwrap_or(u32::MAX));
    let (actual, _, _, components) = value_response(&price, q.usage, bound)?;
    let actual = actual.ok_or(InferenceError::Configuration)?;
    // The floor the response's own usage proved is never undercut.
    if row.floor_microusd.is_some_and(|f| actual < f) {
        return Err(InferenceError::InvalidRequest);
    }
    let accounted = unrowed(&mut tx, q.execution, expired, &s).await? == Unrowed::Nothing;
    let [it, ct, ia, ca, ot, oa] = q.wanted;
    let changed = sqlx::query("UPDATE realtime_responses SET state='settled',actual_microusd=$3,cost_components=$4,input_text_tokens=$5,cached_text_tokens=$6,input_audio_tokens=$7,cached_audio_tokens=$8,output_text_tokens=$9,output_audio_tokens=$10,evidence=$11,resolved_at=clock_timestamp() WHERE execution_id=$1 AND sequence=$2 AND state='unknown'")
        .bind(q.execution).bind(q.sequence).bind(actual).bind(components.map(|c| c.to_value()))
        .bind(it).bind(ct).bind(ia).bind(ca).bind(ot).bind(oa).bind(q.evidence)
        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    let t = totals(&mut tx, q.execution).await?;
    let settles = accounted && t.responses == t.settled;
    if settles {
        settle_session(&mut tx, &r, q.execution, q.evidence, &t, 0).await?;
    }
    sqlx::query("INSERT INTO audit_events(id,workspace_id,actor_user_id,action,resource_type,resource_id,metadata,admin_token_id) VALUES($1,$2,$3,'realtime_response.reconciled','execution',$4,jsonb_build_object('sequence',$5::int,'session_settled',$6::boolean),$7)")
        .bind(Uuid::new_v4()).bind(q.workspace).bind(q.actor.user_id).bind(q.execution).bind(q.sequence).bind(settles).bind(q.actor.token_id)
        .execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)?;
    Ok(if settles {
        Resolved::Session
    } else {
        Resolved::Response
    })
}

/// The session's last unknown response settled: the session settles once,
/// with the responses' sum (`finish` would have written the same) and one
/// `reconciliation` ledger entry carrying this resolution's evidence.
async fn settle_session(
    tx: &mut Tx<'_>,
    r: &settlement::Reservation,
    execution: Uuid,
    evidence: &str,
    t: &Totals,
    uncounted: i64,
) -> Result<(), InferenceError> {
    // The per-key sum totals the actual only when every settled response
    // recorded its components (a settled row may have none); otherwise the
    // session records its actual without them.
    let complete: bool = sqlx::query_scalar("SELECT coalesce(bool_and(cost_components IS NOT NULL),false) FROM realtime_responses WHERE execution_id=$1 AND state='settled'")
        .bind(execution)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)?;
    let components = t.components.as_ref().filter(|_| complete);
    let meters = MeterUsage {
        output_images: Some(0),
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some((t.responses - uncounted) as u64),
        ..MeterUsage::default()
    };
    let meter_json = serde_json::to_value(meters).map_err(|_| InferenceError::Storage)?;
    lock_rows(tx, &[r.touch()], None).await?;
    let settled = sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=$3,input_tokens=$4,output_tokens=$5,billing_usage=NULL,cost_components=$6,meter_usage=$7 WHERE execution_id=$1 AND admitted_at=$2 AND state='unknown'")
        .bind(execution).bind(r.admitted_at).bind(t.actual).bind(t.input).bind(t.output).bind(components).bind(&meter_json)
        .execute(&mut **tx).await.map_err(storage)?.rows_affected();
    if settled != 1 {
        return Err(InferenceError::Storage);
    }
    sqlx::query("UPDATE inference_executions SET input_tokens=$3,output_tokens=$4,meter_usage=$5 WHERE id=$1 AND started_at=$2")
        .bind(execution).bind(r.admitted_at).bind(t.input).bind(t.output).bind(&meter_json)
        .execute(&mut **tx).await.map_err(storage)?;
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,admitted_at) VALUES($1,$2,'reconciliation',$3,$4,$5,NULL,$6,$7,$8,$9)")
        .bind(Uuid::now_v7()).bind(execution).bind(t.actual).bind(t.input).bind(t.output).bind(components).bind(evidence).bind(&meter_json).bind(r.admitted_at)
        .execute(&mut **tx).await.map_err(storage)?;
    Ok(())
}

/// Why a session resolution changed nothing (each refusal rolls back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRefusal {
    /// The session is still open.
    Open,
    /// A response is still unknown: resolve it first, one by one.
    UnknownResponses,
    /// The hold holds a window with no response row: state its usage.
    RemainderRequired,
    /// The hold cannot be attributed to the session's rows and one window.
    Unattributed,
}

/// Settle a finished realtime session none of whose responses is unknown,
/// from an operator's evidence (#804): the session is idle-killed (every
/// response settled, the reservation left unknown by lease expiry) or its
/// last responses were resolved without settling it. When its hold is
/// exactly what its rows account for ([`unrowed`]), `remainder` must be
/// `None` and it settles at the sum of its responses. When it holds one
/// window with no row, `remainder` states that request's usage (all zero is
/// allowed): it is valued with the pinned price like any response and
/// recorded as a resolved response row holding that window, and then the
/// session settles. Both write the same settlement as the last per-response
/// resolution: one `reconciliation` ledger entry and one audit event.
///
/// `Ok(Err(_))` is a refusal ([`SessionRefusal`]); `InvalidRequest` as in
/// [`resolve_response`], and for a remainder with no window to attribute it
/// to or a session already settled with other evidence (the identical
/// request again is [`Resolved::Replay`]); `Configuration` for a remainder
/// without a complete pinned valuation. Lock order as [`resolve_response`]
/// (no response row is locked: none is unknown, and the reservation row
/// serializes every writer of this session's rows).
pub async fn resolve_session(
    store: &Store,
    workspace: Uuid,
    execution: Uuid,
    remainder: Option<RealtimeUsage>,
    evidence: &str,
    actor: impl Into<super::ReconcilingActor>,
) -> Result<Result<Resolved, SessionRefusal>, InferenceError> {
    let actor = actor.into();
    if !valid_evidence(evidence) {
        return Err(InferenceError::InvalidRequest);
    }
    let remainder = remainder.map(|u| stored(u).map(|c| (u, c))).transpose()?;
    let request = SessionRequest {
        workspace,
        execution,
        remainder,
        evidence,
        actor,
    };
    locks::retry_deadlocks!(
        "reconciliation",
        resolve_session_once(store, &request).await
    )
}

struct SessionRequest<'a> {
    workspace: Uuid,
    execution: Uuid,
    remainder: Option<(RealtimeUsage, [i64; 6])>,
    evidence: &'a str,
    actor: super::ReconcilingActor,
}

async fn resolve_session_once(
    store: &Store,
    q: &SessionRequest<'_>,
) -> Result<Result<Resolved, SessionRefusal>, InferenceError> {
    let mut tx = super::reconcile::operator_tx(store, q.workspace, q.actor).await?;
    let r = reservation(&mut tx, q.execution).await?;
    if r.workspace_id != q.workspace || r.workload_kind != WorkloadKind::Realtime.as_str() {
        return Err(InferenceError::InvalidRequest);
    }
    super::reconcile::operator_authorized(&mut tx, q.actor, q.workspace).await?;
    let s = session(&mut tx, q.execution).await?;
    let (open, expired): (bool, bool) = sqlx::query_as(
        "SELECT state='started',error_code IS NOT DISTINCT FROM 'lease_expired' FROM inference_executions WHERE id=$1 AND started_at=$2",
    )
    .bind(q.execution)
    .bind(r.admitted_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    if r.state == "pending" || open {
        return Ok(Err(SessionRefusal::Open));
    }
    if r.state == "settled" {
        return replay(&mut tx, &r, q).await.map(Ok);
    }
    let unresolved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM realtime_responses WHERE execution_id=$1 AND state<>'settled')",
    )
    .bind(q.execution)
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    if unresolved {
        return Ok(Err(SessionRefusal::UnknownResponses));
    }
    let remainder = match (
        unrowed(&mut tx, q.execution, expired, &s).await?,
        q.remainder,
    ) {
        (Unrowed::Unattributed, _) => return Ok(Err(SessionRefusal::Unattributed)),
        (Unrowed::Window(_), None) => return Ok(Err(SessionRefusal::RemainderRequired)),
        (Unrowed::Nothing, Some(_)) => return Err(InferenceError::InvalidRequest),
        (Unrowed::Nothing, None) => None,
        (Unrowed::Window(window), Some(usage)) => {
            Some(record_remainder(&mut tx, &s, q, window, usage).await?)
        }
    };
    let t = totals(&mut tx, q.execution).await?;
    if t.responses != t.settled {
        return Err(InferenceError::Storage);
    }
    // A remainder stated as all zero is a request that was never served: it
    // is recorded (its window and evidence) but counts no request.
    let uncounted = i64::from(q.remainder.is_some_and(|(_, c)| c == [0; 6]));
    settle_session(&mut tx, &r, q.execution, q.evidence, &t, uncounted).await?;
    sqlx::query("INSERT INTO audit_events(id,workspace_id,actor_user_id,action,resource_type,resource_id,metadata,admin_token_id) VALUES($1,$2,$3,'realtime_session.reconciled','execution',$4,jsonb_build_object('remainder_sequence',$5::int),$6)")
        .bind(Uuid::new_v4()).bind(q.workspace).bind(q.actor.user_id).bind(q.execution).bind(remainder).bind(q.actor.token_id)
        .execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)?;
    Ok(Ok(Resolved::Session))
}

/// A settled session: only the identical session resolution again is
/// success, and nothing is written either way. Identical: the session was
/// settled by this route (its `realtime_session.reconciled` event) with this
/// evidence (its `reconciliation` entry), and either both stated no
/// remainder or the remainder row the event names has this evidence and
/// these counters.
async fn replay(
    tx: &mut Tx<'_>,
    r: &settlement::Reservation,
    q: &SessionRequest<'_>,
) -> Result<Resolved, InferenceError> {
    let same: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM monetary_ledger WHERE execution_id=$1 AND admitted_at=$2 AND kind='reconciliation' AND evidence=$3) AND EXISTS(SELECT 1 FROM audit_events a WHERE a.action='realtime_session.reconciled' AND a.resource_type='execution' AND a.resource_id=$1 AND CASE WHEN $4::bigint[] IS NULL THEN jsonb_typeof(a.metadata->'remainder_sequence')='null' ELSE EXISTS(SELECT 1 FROM realtime_responses x WHERE x.execution_id=$1 AND x.sequence=(a.metadata->>'remainder_sequence')::int AND x.evidence=$3 AND ARRAY[x.input_text_tokens,x.cached_text_tokens,x.input_audio_tokens,x.cached_audio_tokens,x.output_text_tokens,x.output_audio_tokens]=$4::bigint[]) END)")
        .bind(q.execution)
        .bind(r.admitted_at)
        .bind(q.evidence)
        .bind(q.remainder.map(|(_, c)| c.to_vec()))
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)?;
    if same {
        Ok(Resolved::Replay)
    } else {
        Err(InferenceError::InvalidRequest)
    }
}

/// The unrowed request's usage, valued with the pinned price like any
/// response (all zero: a request never served, which meters no request), as
/// a resolved response row holding the unrowed window (so [`unrowed`] then
/// finds nothing beyond the rows). Its sequence follows the session's last.
async fn record_remainder(
    tx: &mut Tx<'_>,
    s: &Session,
    q: &SessionRequest<'_>,
    window: i64,
    (usage, counts): (RealtimeUsage, [i64; 6]),
) -> Result<i32, InferenceError> {
    let price = pinned(tx, s).await?.ok_or(InferenceError::Configuration)?;
    if price.pricing_version != 3 {
        return Err(InferenceError::Configuration);
    }
    let bound = ResponseBound::unknown(u32::try_from(usage.output_tokens()).unwrap_or(u32::MAX));
    let requests = u64::from(counts != [0; 6]);
    let (actual, _, _, components) = value_requests(&price, usage, bound, requests)?;
    let actual = actual.ok_or(InferenceError::Configuration)?;
    // A window the price could not bound held nothing: recorded as such
    // (no window, unbounded), as `open_response` records one.
    let unbounded = window == 0 && s.unbounded_cost;
    let window = (!unbounded).then_some(window);
    let [it, ct, ia, ca, ot, oa] = counts;
    sqlx::query_scalar("INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,unbounded_cost,actual_microusd,cost_components,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,evidence,resolved_at,completed_at) SELECT $1,coalesce(max(sequence),0)+1,'settled',$2,$12,$3,$4,$5,$6,$7,$8,$9,$10,$11,clock_timestamp(),clock_timestamp() FROM realtime_responses WHERE execution_id=$1 RETURNING sequence")
        .bind(q.execution).bind(window).bind(actual).bind(components.map(|c| c.to_value()))
        .bind(it).bind(ct).bind(ia).bind(ca).bind(ot).bind(oa).bind(q.evidence).bind(unbounded)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)
}
