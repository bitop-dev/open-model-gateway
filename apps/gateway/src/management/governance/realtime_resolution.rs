//! Platform Admin resolution of realtime sessions left unknown (#804): one
//! unknown response from evidence, or a session none of whose responses is
//! unknown (`governance::realtime_resolution`).
use super::*;
/// `POST …/costs/{execution}/realtime-responses/{sequence}/reconcile` (#804):
/// a Platform Admin settles one unknown response of a finished realtime
/// session from evidence. The six counters are decimal strings, like the
/// request-level route's; the answer carries no usage or evidence.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RealtimeReconcile {
    input_text_tokens: String,
    cached_text_tokens: String,
    input_audio_tokens: String,
    cached_audio_tokens: String,
    output_text_tokens: String,
    output_audio_tokens: String,
    evidence: String,
}
pub(super) async fn reconcile_realtime_response(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path((ws, execution, sequence)): Path<(Uuid, Uuid, i32)>,
    Json(p): Json<RealtimeReconcile>,
) -> ApiResult {
    let (mut tx, _) = resources::workspace_tx(&s, &u, ws).await?;
    resources::platform_write(&mut tx, &u).await?;
    // A realtime session of this workspace with that response; anything else
    // is not found (a foreign personal workspace already was, above).
    sqlx::query_scalar::<_, Uuid>(
        "SELECT e.id FROM inference_executions e JOIN realtime_responses x ON x.execution_id=e.id AND x.sequence=$3 WHERE e.workspace_id=$1 AND e.id=$2 AND e.workload_kind='realtime'",
    )
    .bind(ws)
    .bind(execution)
    .bind(sequence)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    tx.commit().await?;
    let usage = realtime_usage([
        &p.input_text_tokens,
        &p.cached_text_tokens,
        &p.input_audio_tokens,
        &p.cached_audio_tokens,
        &p.output_text_tokens,
        &p.output_audio_tokens,
    ])?;
    crate::governance::realtime_resolution::resolve_response(
        &s,
        ws,
        execution,
        sequence,
        usage,
        &p.evidence,
        crate::governance::ReconcilingActor {
            user_id: u.user_id(),
            token_id: u.token_id(),
        },
    )
    .await
    .map_err(|e| match e {
        InferenceError::InvalidRequest => invalid(),
        InferenceError::Busy => ApiError(
            StatusCode::CONFLICT,
            "The realtime session or response is still open",
        ),
        InferenceError::Configuration => ApiError(
            StatusCode::CONFLICT,
            "Response has no complete pinned valuation",
        ),
        InferenceError::Unauthenticated => denied(),
        _ => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Reconciliation unavailable",
        ),
    })?;
    Ok(ok())
}
/// The six realtime counters, canonical decimal strings, in column order.
fn realtime_usage(
    [it, ct, ia, ca, ot, oa]: [&String; 6],
) -> Result<crate::inference::realtime::RealtimeUsage, ApiError> {
    let count = |v: &str| money(v, false).map(|n| n as u64);
    Ok(crate::inference::realtime::RealtimeUsage {
        input_text_tokens: count(it)?,
        cached_text_tokens: count(ct)?,
        input_audio_tokens: count(ia)?,
        cached_audio_tokens: count(ca)?,
        output_text_tokens: count(ot)?,
        output_audio_tokens: count(oa)?,
    })
}
/// `POST …/costs/{execution}/realtime-session/reconcile` (#804): a Platform
/// Admin settles a finished realtime session none of whose responses is
/// unknown, from evidence; `remainder` states the usage of a request whose
/// window the session holds without a response row.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RealtimeSessionReconcile {
    evidence: String,
    #[serde(default)]
    remainder: Option<RealtimeCounters>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RealtimeCounters {
    input_text_tokens: String,
    cached_text_tokens: String,
    input_audio_tokens: String,
    cached_audio_tokens: String,
    output_text_tokens: String,
    output_audio_tokens: String,
}
pub(super) async fn reconcile_realtime_session(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path((ws, execution)): Path<(Uuid, Uuid)>,
    Json(p): Json<RealtimeSessionReconcile>,
) -> ApiResult {
    use crate::governance::realtime_resolution::SessionRefusal;
    let (mut tx, _) = resources::workspace_tx(&s, &u, ws).await?;
    resources::platform_write(&mut tx, &u).await?;
    // A realtime session of this workspace; anything else is not found (a
    // foreign personal workspace already was, above).
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM inference_executions WHERE workspace_id=$1 AND id=$2 AND workload_kind='realtime'",
    )
    .bind(ws)
    .bind(execution)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    tx.commit().await?;
    let remainder = p
        .remainder
        .as_ref()
        .map(|c| {
            realtime_usage([
                &c.input_text_tokens,
                &c.cached_text_tokens,
                &c.input_audio_tokens,
                &c.cached_audio_tokens,
                &c.output_text_tokens,
                &c.output_audio_tokens,
            ])
        })
        .transpose()?;
    let conflict = |message| ApiError(StatusCode::CONFLICT, message);
    crate::governance::realtime_resolution::resolve_session(
        &s,
        ws,
        execution,
        remainder,
        &p.evidence,
        crate::governance::ReconcilingActor {
            user_id: u.user_id(),
            token_id: u.token_id(),
        },
    )
    .await
    .map_err(|e| match e {
        InferenceError::InvalidRequest => invalid(),
        InferenceError::Configuration => conflict("Remainder has no complete pinned valuation"),
        InferenceError::Unauthenticated => denied(),
        _ => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Reconciliation unavailable",
        ),
    })?
    .map_err(|refusal| {
        conflict(match refusal {
            SessionRefusal::Open => "The realtime session is still open",
            SessionRefusal::UnknownResponses => "Resolve the session's unknown responses first",
            SessionRefusal::RemainderRequired => {
                "The session holds a request with no response: state its usage as the remainder"
            }
            SessionRefusal::Unattributed => {
                "The session's hold cannot be attributed to its responses"
            }
        })
    })?;
    Ok(ok())
}
