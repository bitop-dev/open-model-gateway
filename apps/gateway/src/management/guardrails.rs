//! Guardrail policy routes (#840; docs/guardrails-design.md §5, "Management
//! API sketch"): the workspace-type default, the platform per-workspace override
//! and the workspace local layer, plus the detector list.
//!
//! Who may do what:
//! - Platform Admins write type defaults and overrides; Platform Admins and
//!   Auditors read them (custom pattern text included).
//! - The owners and admins of a shared workspace, and the owner of a personal
//!   one, write its local layer and read its patterns.
//! - Members (and, on a shared workspace, platform staff) read the effective
//!   rules; custom patterns they may not read appear as a count.
//! - Platform staff never see a personal workspace's local layer: it is an
//!   owner-made choice. They see its platform layers.
//! - Inference keys are never management credentials, and admin tokens never
//!   write a local layer.
//!
//! Writes run in the management transaction (the shared catalog lock, then the
//! installation row), which serializes them. They take no authority lock:
//! admission never reads guardrails. The `guardrails` topic is bumped at commit
//! by the tables' triggers (0092).

use super::*;
use crate::guardrails::{
    patterns,
    policy::{self, Entry, Layer, LayerIn, Refusal, RuleSet, StoredRule},
};
use std::collections::{BTreeSet, HashSet};

pub(super) const DETECTOR_UNAVAILABLE: &str = "This guardrail rule needs a detector, action, output mode or workload this gateway does not provide yet";
pub(super) const EGRESS_NOT_ALLOWED: &str =
    "The platform keeps guardrail inspection local; this detector sends text out of the gateway";
pub(super) const PATTERN_INVALID: &str =
    "A custom pattern does not compile within the guardrail pattern limits";
pub(super) const LOOSEN_NOT_ALLOWED: &str =
    "A workspace may tighten a platform guardrail rule, never weaken, replace or remove it";

/// A management error, or the one refusal that names a rule.
pub(super) enum GuardrailError {
    Api(ApiError),
    Loosen(Uuid),
}
impl<E: Into<ApiError>> From<E> for GuardrailError {
    fn from(e: E) -> Self {
        Self::Api(e.into())
    }
}
impl IntoResponse for GuardrailError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(e) => e.into_response(),
            // The shared envelope (status, message, reason) plus the platform
            // rule: its id only, never its content.
            Self::Loosen(rule) => {
                let mut body =
                    json!({"error":{"code":"403","message":LOOSEN_NOT_ALLOWED,"rule":rule}});
                if let Some((_, reason)) = super::errors::REASONS
                    .iter()
                    .find(|(m, _)| *m == LOOSEN_NOT_ALLOWED)
                {
                    body["error"]["reason"] = json!(reason);
                }
                (StatusCode::FORBIDDEN, Json(body)).into_response()
            }
        }
    }
}
type Result<T> = std::result::Result<T, GuardrailError>;

fn refused(r: Refusal) -> GuardrailError {
    match r {
        Refusal::Invalid => GuardrailError::Api(invalid()),
        Refusal::DetectorUnavailable => {
            GuardrailError::Api(ApiError(StatusCode::BAD_REQUEST, DETECTOR_UNAVAILABLE))
        }
        Refusal::EgressNotAllowed => {
            GuardrailError::Api(ApiError(StatusCode::FORBIDDEN, EGRESS_NOT_ALLOWED))
        }
        Refusal::PatternInvalid => {
            GuardrailError::Api(ApiError(StatusCode::BAD_REQUEST, PATTERN_INVALID))
        }
        Refusal::LoosenNotAllowed(rule) => GuardrailError::Loosen(rule),
    }
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/guardrails/detectors", get(detectors))
        .route(
            "/api/v1/platform/workspace-types/{kind}/guardrails",
            get(type_guardrails).put(put_type_guardrails),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/guardrails",
            get(platform_workspace_guardrails)
                .put(put_platform_workspace_guardrails)
                .delete(reset_platform_workspace_guardrails),
        )
        .route(
            "/api/v1/workspaces/{ws}/guardrails",
            get(workspace_guardrails).put(put_workspace_guardrails),
        )
}

mod store;
use store::{Platform, Scope, foreign_ids, load, platform, store};

/// Pattern compiles per replica, in separate slots for the platform layers
/// (Platform Admins) and the local layers (every User owns one, on their
/// personal workspace), so local writers never queue ahead of the platform.
static PLATFORM_COMPILES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
static LOCAL_COMPILES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Run `work` on the blocking pool while holding one of `slots`. The slot
/// moves into the blocking task, so it is held until the work ends even when
/// the request is dropped (a client that disconnects never frees a slot while
/// its compile still runs).
async fn in_slot<T: Send + 'static>(
    slots: &'static tokio::sync::Semaphore,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let unavailable = || {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Management storage unavailable",
        )
    };
    let slot = slots.acquire().await.map_err(|_| unavailable())?;
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .map_err(|_| unavailable().into())
}

/// The body's custom patterns that compile within the limits. Compiling needs
/// no stored state, so it runs before the management transaction (never while
/// it holds the installation row), off the async workers and in a bounded
/// number of slots. The route policy layer has already refused a caller who
/// may not write this layer, before the body was read. Validation reaches at
/// most the first `MAX_PATTERNS` patterns in body order, so only those are
/// compiled, each distinct one once; any other pattern counts as not
/// compiling.
async fn compiled(body: &LayerIn, layer: Layer) -> Result<HashSet<String>> {
    let wanted: BTreeSet<String> = body.patterns(policy::MAX_PATTERNS).into_iter().collect();
    if wanted.is_empty() {
        return Ok(HashSet::new());
    }
    let slots = if layer == Layer::WorkspaceLocal {
        &LOCAL_COMPILES
    } else {
        &PLATFORM_COMPILES
    };
    in_slot(slots, move || {
        wanted.into_iter().filter(|p| patterns::valid(p)).collect()
    })
    .await
}

/// Validate a PUT body against its layer.
async fn validated(
    tx: &mut Transaction<'_, Postgres>,
    body: LayerIn,
    compiled: &HashSet<String>,
    layer: Layer,
    platform: Option<&RuleSet>,
    current: Option<&RuleSet>,
) -> Result<(Option<bool>, Vec<StoredRule>)> {
    let wanted: Vec<Uuid> = body.ids();
    let foreign = foreign_ids(tx, current, &wanted).await?;
    policy::validate(
        body,
        &policy::WriteContext {
            layer,
            platform,
            current,
            foreign_ids: &foreign,
            compile: &|p| compiled.contains(p),
        },
    )
    .map_err(refused)
}

// --- Audit -------------------------------------------------------------------

/// `guardrail_policy.updated`: the layer, its revision, and each entry's id,
/// action and output mode; custom patterns only as a count.
async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Option<Uuid>,
    scope: Scope<'_>,
    revision: Option<i64>,
    local_only: Option<bool>,
    rules: &[StoredRule],
) -> std::result::Result<(), ApiError> {
    let summary: Vec<Value> = rules
        .iter()
        .map(|r| match &r.entry {
            Entry::Rule(rule) => json!({"id": r.id, "action": rule.input_action.as_str(), "output_mode": rule.output_mode.as_str()}),
            Entry::Tightens(target, t) => json!({"id": r.id, "tightens": target, "action": t.input_action.map(|a| a.as_str()), "output_mode": t.output_mode.map(|m| m.as_str())}),
        })
        .collect();
    let patterns: usize = rules
        .iter()
        .map(|r| match &r.entry {
            Entry::Rule(rule) => rule.detector.patterns.len(),
            Entry::Tightens(..) => 0,
        })
        .sum();
    let mut metadata = json!({
        "layer": scope.layer().as_str(),
        "revision": revision,
        "rules": summary,
        "custom_patterns": patterns,
        "count": rules.len(),
        "reset": revision.is_none(),
    });
    if let Some(l) = local_only {
        metadata["local_only"] = json!(l);
    }
    if let Scope::Type(k) = scope {
        metadata["kind"] = json!(k);
    }
    let (kind, id) = match scope {
        Scope::Type(_) => ("workspace_type", None),
        Scope::Override(w) | Scope::Local(w) => ("workspace", Some(w)),
    };
    resources::audit(tx, u, ws, "guardrail_policy.updated", kind, id, metadata).await
}

/// `guardrail_policy.updated` audit metadata, key by key: each entry's id, the
/// platform rule a tightening entry names, and its closed action and output
/// mode codes; the layer, revision and flags; a pattern count. Never a rule's
/// name or a custom pattern.
pub(super) fn audit_field(action: &str, k: &str, v: &Value) -> bool {
    if action != "guardrail_policy.updated" {
        return false;
    }

    let closed = |v: Option<&Value>, allowed: &[&str]| {
        v.is_none_or(|v| v.is_null() || v.as_str().is_some_and(|s| allowed.contains(&s)))
    };
    let id = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
    };
    match k {
        "layer" => v.as_str().is_some_and(|s| {
            matches!(s, "type_default" | "workspace_override" | "workspace_local")
        }),
        "revision" => v.is_null() || v.is_u64(),
        "custom_patterns" => v.is_u64(),
        "local_only" | "reset" => v.is_boolean(),
        "rules" => v.as_array().is_some_and(|rules| {
            rules.len() <= crate::guardrails::policy::MAX_RULES
                && rules.iter().all(|r| {
                    r.as_object().is_some_and(|o| {
                        o.keys().all(|k| {
                            matches!(k.as_str(), "id" | "tightens" | "action" | "output_mode")
                        }) && id(o.get("id"))
                            && (o.get("tightens").is_none() || id(o.get("tightens")))
                            && closed(o.get("action"), &["off", "monitor", "redact", "block"])
                            && closed(o.get("output_mode"), &["off", "monitor", "stop", "block"])
                    })
                })
        }),
        _ => false,
    }
}

// --- Responses ---------------------------------------------------------------

fn set_json(set: Option<&RuleSet>, show_patterns: bool) -> Value {
    set.map_or(Value::Null, |s| {
        json!({
            "id": s.id,
            "layer": s.layer.as_str(),
            "revision": s.revision,
            "local_only": s.local_only,
            "rules": s.rules.iter().map(|r| policy::rule_json(r.id, &r.entry, show_patterns)).collect::<Vec<_>>(),
        })
    })
}

/// The workspace view: its platform layer, its local layer and the effective
/// rules. `local_visible` is false only for platform staff on a personal
/// workspace (owner-made choice); the effective list is then omitted.
fn workspace_json(
    ws: Uuid,
    p: &Platform,
    local: Option<&RuleSet>,
    local_visible: bool,
    platform_patterns: bool,
    local_patterns: bool,
) -> Value {
    let current = p.current();
    let effective = local_visible.then(|| {
        policy::effective_json(&policy::compose(current, local), |layer| {
            if layer == Layer::WorkspaceLocal {
                local_patterns
            } else {
                platform_patterns
            }
        })
    });
    json!({
        "workspace_id": ws,
        "kind": p.kind,
        "platform": {
            "source": if p.workspace_override.is_some() { "workspace_override" } else { "type_default" },
            "rule_set": set_json(current, platform_patterns),
        },
        "egress": {"local_only": current.and_then(|s| s.local_only).unwrap_or(false)},
        "local": if local_visible { set_json(local, local_patterns) } else { Value::Null },
        "local_visible": local_visible,
        "effective": effective,
        // What this release inspects: nothing yet (#840 policy model only).
        "enforced": true,
    })
}

fn kind(k: &str) -> std::result::Result<(), ApiError> {
    if matches!(k, "personal" | "team" | "project") {
        Ok(())
    } else {
        Err(invalid())
    }
}

async fn platform_tx<'a>(
    s: &'a Store,
    u: &impl Actor,
    write: bool,
) -> std::result::Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    if write {
        resources::platform_write(&mut tx, u).await?;
    } else {
        resources::platform_read(&mut tx, u).await?;
    }
    Ok(tx)
}

// --- Handlers ------------------------------------------------------------------

async fn detectors(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
) -> Result<Json<Value>> {
    let tx = platform_tx(&s, &u, false).await?;
    tx.commit().await?;
    Ok(Json(json!({"detectors": policy::detectors_json()})))
}

async fn type_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(k): Path<String>,
) -> Result<Json<Value>> {
    kind(&k)?;
    let mut tx = platform_tx(&s, &u, false).await?;
    let set = load(&mut tx, Scope::Type(&k)).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"kind": k, "rule_set": set_json(set.as_ref(), true)}),
    ))
}

async fn put_type_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(k): Path<String>,
    Json(body): Json<LayerIn>,
) -> Result<Json<Value>> {
    kind(&k)?;
    let compiled = compiled(&body, Layer::TypeDefault).await?;
    let mut tx = platform_tx(&s, &u, true).await?;
    let scope = Scope::Type(&k);
    let current = load(&mut tx, scope).await?;
    let (local_only, rules) = validated(
        &mut tx,
        body,
        &compiled,
        Layer::TypeDefault,
        None,
        current.as_ref(),
    )
    .await?;
    let (_, revision) = store(
        &mut tx,
        scope,
        current.as_ref(),
        local_only,
        &rules,
        u.user_id(),
    )
    .await?;
    audit(&mut tx, &u, None, scope, Some(revision), local_only, &rules).await?;
    let set = load(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"kind": k, "rule_set": set_json(set.as_ref(), true)}),
    ))
}

async fn platform_view(tx: &mut Transaction<'_, Postgres>, ws: Uuid) -> Result<Value> {
    let p = platform(tx, ws).await?;
    // A personal workspace's local layer is the owner's own choice.
    let local_visible = p.kind != "personal";
    let local = if local_visible {
        load(tx, Scope::Local(ws)).await?
    } else {
        None
    };
    let mut v = workspace_json(ws, &p, local.as_ref(), local_visible, true, true);
    v["type_default"] = set_json(p.type_default.as_ref(), true);
    v["override"] = set_json(p.workspace_override.as_ref(), true);
    Ok(v)
}

async fn platform_workspace_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(ws): Path<Uuid>,
) -> Result<Json<Value>> {
    let mut tx = platform_tx(&s, &u, false).await?;
    let v = platform_view(&mut tx, ws).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn put_platform_workspace_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(ws): Path<Uuid>,
    Json(body): Json<LayerIn>,
) -> Result<Json<Value>> {
    let compiled = compiled(&body, Layer::WorkspaceOverride).await?;
    let mut tx = platform_tx(&s, &u, true).await?;
    platform(&mut tx, ws).await?;
    let scope = Scope::Override(ws);
    let current = load(&mut tx, scope).await?;
    let (local_only, rules) = validated(
        &mut tx,
        body,
        &compiled,
        Layer::WorkspaceOverride,
        None,
        current.as_ref(),
    )
    .await?;
    let (_, revision) = store(
        &mut tx,
        scope,
        current.as_ref(),
        local_only,
        &rules,
        u.user_id(),
    )
    .await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        scope,
        Some(revision),
        local_only,
        &rules,
    )
    .await?;
    let v = platform_view(&mut tx, ws).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn reset_platform_workspace_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(ws): Path<Uuid>,
) -> Result<Json<Value>> {
    let mut tx = platform_tx(&s, &u, true).await?;
    platform(&mut tx, ws).await?;
    if let Some(set) = load(&mut tx, Scope::Override(ws)).await? {
        // The removal is made by the Platform Admin who moves the revision:
        // the database removes an override's rules and the override only in
        // that transaction (0092).
        sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
            .bind(set.id)
            .bind(u.user_id())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
            .bind(set.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM guardrail_rule_sets WHERE id=$1")
            .bind(set.id)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, &u, Some(ws), Scope::Override(ws), None, None, &[]).await?;
    }
    let v = platform_view(&mut tx, ws).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn workspace_view(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    a: &resources::WorkspaceAccess,
) -> Result<Value> {
    let p = platform(tx, ws).await?;
    let local = load(tx, Scope::Local(ws)).await?;
    // Pattern text for those who may read that layer's configuration: the
    // layer's writers, and platform staff for the platform layer (and for a
    // shared workspace's local layer). Everyone else sees a count.
    let shared = a.kind != "personal";
    let v = workspace_json(
        ws,
        &p,
        local.as_ref(),
        true,
        a.platform_reader,
        a.admin || (a.platform_reader && shared),
    );
    Ok(v)
}

async fn workspace_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(ws): Path<Uuid>,
) -> Result<Json<Value>> {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    a.metadata_read()?;
    let mut v = workspace_view(&mut tx, ws, &a).await?;
    v["can_edit"] = json!(a.admin);
    tx.commit().await?;
    Ok(Json(v))
}

async fn put_workspace_guardrails(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(body): Json<LayerIn>,
) -> Result<Json<Value>> {
    let compiled = compiled(&body, Layer::WorkspaceLocal).await?;
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    // Owners and admins of a shared workspace; the owner of a personal one.
    if !a.admin {
        return Err(denied().into());
    }
    let p = platform(&mut tx, ws).await?;
    let scope = Scope::Local(ws);
    let current = load(&mut tx, scope).await?;
    let (_, rules) = validated(
        &mut tx,
        body,
        &compiled,
        Layer::WorkspaceLocal,
        p.current(),
        current.as_ref(),
    )
    .await?;
    let (_, revision) = store(&mut tx, scope, current.as_ref(), None, &rules, u.user_id()).await?;
    audit(&mut tx, &u, Some(ws), scope, Some(revision), None, &rules).await?;
    let mut v = workspace_view(&mut tx, ws, &a).await?;
    v["can_edit"] = json!(true);
    tx.commit().await?;
    Ok(Json(v))
}

#[cfg(test)]
mod slot_tests {
    use super::*;

    static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

    #[tokio::test]
    async fn a_dropped_request_keeps_its_slot_until_the_work_ends() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (started, running) = tokio::sync::oneshot::channel::<()>();
        let request = tokio::spawn(in_slot(&SLOT, move || {
            started.send(()).unwrap();
            wait.recv().unwrap();
        }));
        running.await.unwrap();
        // The client goes away while the work runs: the slot stays taken.
        request.abort();
        assert!(request.await.is_err_and(|e| e.is_cancelled()));
        assert_eq!(SLOT.available_permits(), 0);
        release.send(()).unwrap();
        // Freed once the blocking work itself has ended.
        let _again = tokio::time::timeout(std::time::Duration::from_secs(5), SLOT.acquire())
            .await
            .expect("the slot is freed when the work ends")
            .unwrap();
    }
}
