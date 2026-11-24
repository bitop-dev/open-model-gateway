//! Operator resolution of a finished realtime session's unknown responses
//! (#804). Each unknown response is settled from evidence, valued with the
//! session's pinned price exactly as `response.done` would have been; the
//! session settles once its last unknown response is resolved, with one
//! `reconciliation` ledger entry. The refusals mirror request resolution
//! ([`super::resolve_usage`]).
use super::realtime::{Session, Totals, pinned, session, totals, value_response};
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
/// workspace and user authority (or the installation lock in global mode),
/// then the session's reservation row, then the response row, then the
/// session's totals rows. Two resolutions of one session therefore serialize
/// on its reservation row: the second sees what the first wrote, and exactly
/// one settles the session.
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
    if evidence.trim().is_empty()
        || evidence.chars().count() > 200
        || evidence.chars().any(char::is_control)
        || !usage.valid()
    {
        return Err(InferenceError::InvalidRequest);
    }
    let wanted = counters(usage).map(|n| i64::try_from(n).ok());
    let Some(wanted) = wanted.iter().copied().collect::<Option<Vec<i64>>>() else {
        return Err(InferenceError::InvalidRequest);
    };
    let wanted: [i64; 6] = wanted
        .try_into()
        .map_err(|_| InferenceError::InvalidRequest)?;
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

/// Whether a session closed by lease expiry may settle (`finish` gives every
/// forwarded request a response row, so a session it closed always may). One
/// closed by lease expiry may still hold a reserved window with no row (a
/// request forwarded before the process stopped, which the provider may have
/// run): it settles only while its hold is exactly what its rows put into it.
/// A row settled by `response.done` put in its actual; any other row
/// (unknown, or settled by a resolution, which never changes the hold) its
/// window or floor, whichever is greater. Windows and floors never change
/// (the response guard), so this sum is the same before and after every
/// resolution. A row without a window (an unbounded price) proves nothing.
async fn accounted(
    tx: &mut Tx<'_>,
    execution: Uuid,
    expired: bool,
    s: &Session,
) -> Result<bool, InferenceError> {
    if !expired {
        return Ok(true);
    }
    let rows: Option<i64> = sqlx::query_scalar("SELECT CASE WHEN bool_or(window_hold_microusd IS NULL AND NOT (state='settled' AND evidence IS NULL)) THEN NULL ELSE coalesce(sum(CASE WHEN state='settled' AND evidence IS NULL THEN actual_microusd ELSE greatest(window_hold_microusd,coalesce(floor_microusd,0)) END),0) END::bigint FROM realtime_responses WHERE execution_id=$1")
        .bind(execution)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(rows.is_some() && rows == s.held_microusd)
}

async fn resolve_response_once(store: &Store, q: &Request<'_>) -> Result<Resolved, InferenceError> {
    let mut tx = super::reconcile::operator_tx(store, q.workspace, q.actor).await?;
    let r = reservation(&mut tx, q.execution, scoped(store)).await?;
    if r.workspace_id != q.workspace || r.workload_kind != WorkloadKind::Realtime.as_str() {
        return Err(InferenceError::InvalidRequest);
    }
    super::reconcile::operator_authorized(store, &mut tx, q.actor, q.workspace).await?;
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
    let accounted = accounted(&mut tx, q.execution, expired, &s).await?;
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
        settle_session(store, &mut tx, &r, q, &t).await?;
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
    store: &Store,
    tx: &mut Tx<'_>,
    r: &settlement::Reservation,
    q: &Request<'_>,
    t: &Totals,
) -> Result<(), InferenceError> {
    // The per-key sum totals the actual only when every settled response
    // recorded its components (a settled row may have none); otherwise the
    // session records its actual without them.
    let complete: bool = sqlx::query_scalar("SELECT coalesce(bool_and(cost_components IS NOT NULL),false) FROM realtime_responses WHERE execution_id=$1 AND state='settled'")
        .bind(q.execution)
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
        requests: Some(t.responses as u64),
        ..MeterUsage::default()
    };
    let meter_json = serde_json::to_value(meters).map_err(|_| InferenceError::Storage)?;
    lock_rows(store, tx, &[r.touch()], false).await?;
    let settled = sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=$3,input_tokens=$4,output_tokens=$5,billing_usage=NULL,cost_components=$6,meter_usage=$7 WHERE execution_id=$1 AND admitted_at=$2 AND state='unknown'")
        .bind(q.execution).bind(r.admitted_at).bind(t.actual).bind(t.input).bind(t.output).bind(components).bind(&meter_json)
        .execute(&mut **tx).await.map_err(storage)?.rows_affected();
    if settled != 1 {
        return Err(InferenceError::Storage);
    }
    sqlx::query("UPDATE inference_executions SET input_tokens=$3,output_tokens=$4,meter_usage=$5 WHERE id=$1 AND started_at=$2")
        .bind(q.execution).bind(r.admitted_at).bind(t.input).bind(t.output).bind(&meter_json)
        .execute(&mut **tx).await.map_err(storage)?;
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,admitted_at) VALUES($1,$2,'reconciliation',$3,$4,$5,NULL,$6,$7,$8,$9)")
        .bind(Uuid::now_v7()).bind(q.execution).bind(t.actual).bind(t.input).bind(t.output).bind(components).bind(q.evidence).bind(&meter_json).bind(r.admitted_at)
        .execute(&mut **tx).await.map_err(storage)?;
    Ok(())
}
