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
use std::collections::HashSet;

use super::*;
use crate::guardrails::{
    patterns,
    policy::{self, Entry, Layer, LayerIn, Refusal, RuleSet, StoredRule},
};

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

// --- Storage -----------------------------------------------------------------

type RuleRow = (
    Uuid,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<String>>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<i32>,
    Option<Vec<String>>,
    Option<String>,
    Vec<String>,
);
const RULE_COLUMNS: &str = "id,tightens_rule_id,name,protection,detector_kind,detector_ref,categories,input_action,redact_scope,output_mode,(threshold*1000)::int,on_error,timeout_ms,workloads,uninspectable,patterns";

fn names<T: Copy + Ord>(
    parse: fn(&str) -> Option<T>,
    v: &[String],
) -> Option<std::collections::BTreeSet<T>> {
    v.iter().map(|s| parse(s)).collect()
}
/// A stored row as an entry. Values come from the table's CHECK constraints;
/// anything unknown is a storage error (never a silently skipped rule).
fn decode(row: RuleRow) -> std::result::Result<StoredRule, ApiError> {
    use policy::*;
    let bad = || {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored guardrail rule is not readable",
        )
    };
    let (
        id,
        tightens,
        name,
        protection,
        kind,
        reference,
        categories,
        input,
        scope,
        output,
        threshold,
        on_error,
        timeout,
        workloads,
        uninspectable,
        pats,
    ) = row;
    let threshold = threshold
        .map(|t| u16::try_from(t).map_err(|_| bad()))
        .transpose()?;
    let entry = match tightens {
        Some(target) => Entry::Tightens(
            target,
            Tightening {
                input_action: input
                    .as_deref()
                    .map(|v| InputAction::parse(v).ok_or_else(bad))
                    .transpose()?,
                output_mode: output
                    .as_deref()
                    .map(|v| OutputMode::parse(v).ok_or_else(bad))
                    .transpose()?,
                on_error: on_error
                    .as_deref()
                    .map(|v| OnError::parse(v).ok_or_else(bad))
                    .transpose()?,
                threshold,
            },
        ),
        None => Entry::Rule(Box::new(Rule {
            name: name.ok_or_else(bad)?,
            protection: protection
                .as_deref()
                .and_then(Protection::parse)
                .ok_or_else(bad)?,
            detector: Detector {
                kind: kind
                    .as_deref()
                    .and_then(DetectorKind::parse)
                    .ok_or_else(bad)?,
                categories: categories
                    .as_deref()
                    .and_then(|c| names(Category::parse, c))
                    .ok_or_else(bad)?,
                patterns: pats,
                reference,
            },
            input_action: input
                .as_deref()
                .and_then(InputAction::parse)
                .ok_or_else(bad)?,
            redact_scope: scope
                .as_deref()
                .and_then(RedactScope::parse)
                .ok_or_else(bad)?,
            output_mode: output
                .as_deref()
                .and_then(OutputMode::parse)
                .ok_or_else(bad)?,
            threshold,
            on_error: on_error
                .as_deref()
                .and_then(OnError::parse)
                .ok_or_else(bad)?,
            timeout_ms: timeout
                .map(|t| u32::try_from(t).map_err(|_| bad()))
                .transpose()?,
            workloads: workloads
                .as_deref()
                .and_then(|w| names(Workload::parse, w))
                .ok_or_else(bad)?,
            uninspectable: uninspectable
                .as_deref()
                .and_then(Uninspectable::parse)
                .ok_or_else(bad)?,
        })),
    };
    Ok(StoredRule { id, entry })
}

/// Where a rule set lives.
#[derive(Clone, Copy)]
enum Scope<'a> {
    Type(&'a str),
    Override(Uuid),
    Local(Uuid),
}
impl Scope<'_> {
    fn layer(self) -> Layer {
        match self {
            Self::Type(_) => Layer::TypeDefault,
            Self::Override(_) => Layer::WorkspaceOverride,
            Self::Local(_) => Layer::WorkspaceLocal,
        }
    }
}

async fn load(
    tx: &mut Transaction<'_, Postgres>,
    scope: Scope<'_>,
) -> std::result::Result<Option<RuleSet>, ApiError> {
    let (kind, ws) = match scope {
        Scope::Type(k) => (Some(k), None),
        Scope::Override(w) | Scope::Local(w) => (None, Some(w)),
    };
    let head: Option<(Uuid, i64, Option<bool>)> = sqlx::query_as(
        "SELECT id,revision,local_only FROM guardrail_rule_sets WHERE layer=$1 AND kind IS NOT DISTINCT FROM $2 AND workspace_id IS NOT DISTINCT FROM $3",
    )
    .bind(scope.layer().as_str())
    .bind(kind)
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, revision, local_only)) = head else {
        return Ok(None);
    };
    let rows: Vec<RuleRow> = sqlx::query_as(&format!(
        "SELECT {RULE_COLUMNS} FROM guardrail_rules WHERE rule_set_id=$1 ORDER BY position"
    ))
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(Some(RuleSet {
        id,
        layer: scope.layer(),
        revision,
        local_only,
        rules: rows
            .into_iter()
            .map(decode)
            .collect::<std::result::Result<_, _>>()?,
    }))
}

/// A workspace's kind and its platform layer: the override if one exists,
/// else its kind's type default.
pub(crate) struct Platform {
    kind: String,
    type_default: Option<RuleSet>,
    workspace_override: Option<RuleSet>,
}
impl Platform {
    fn current(&self) -> Option<&RuleSet> {
        self.workspace_override
            .as_ref()
            .or(self.type_default.as_ref())
    }
}
async fn platform(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
) -> std::result::Result<Platform, ApiError> {
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(Platform {
        type_default: load(tx, Scope::Type(&kind)).await?,
        workspace_override: load(tx, Scope::Override(ws)).await?,
        kind,
    })
}

/// Replace a layer's rules and move its revision forward by one.
async fn store(
    tx: &mut Transaction<'_, Postgres>,
    scope: Scope<'_>,
    current: Option<&RuleSet>,
    local_only: Option<bool>,
    rules: &[StoredRule],
    actor: Uuid,
) -> std::result::Result<(Uuid, i64), ApiError> {
    let (id, revision) = match current {
        Some(set) => {
            sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
                .bind(set.id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,local_only=$2,updated_by=$3 WHERE id=$1")
                .bind(set.id)
                .bind(local_only)
                .bind(actor)
                .execute(&mut **tx)
                .await?;
            (set.id, set.revision + 1)
        }
        None => {
            let id = Uuid::now_v7();
            let (kind, ws) = match scope {
                Scope::Type(k) => (Some(k), None),
                Scope::Override(w) | Scope::Local(w) => (None, Some(w)),
            };
            sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,workspace_id,local_only,updated_by) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(id)
                .bind(scope.layer().as_str())
                .bind(kind)
                .bind(ws)
                .bind(local_only)
                .bind(actor)
                .execute(&mut **tx)
                .await?;
            (id, 1)
        }
    };
    for (position, r) in rules.iter().enumerate() {
        let q = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,name,protection,detector_kind,detector_ref,categories,input_action,redact_scope,output_mode,threshold,on_error,timeout_ms,workloads,uninspectable,patterns) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13::numeric/1000,$14,$15,$16,$17,$18)")
            .bind(r.id)
            .bind(id)
            .bind(i16::try_from(position).map_err(|_| invalid())?);
        let q = match &r.entry {
            Entry::Tightens(target, t) => q
                .bind(Some(*target))
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<Vec<String>>)
                .bind(t.input_action.map(|a| a.as_str()))
                .bind(None::<String>)
                .bind(t.output_mode.map(|m| m.as_str()))
                .bind(t.threshold.map(i32::from))
                .bind(t.on_error.map(|o| o.as_str()))
                .bind(None::<i32>)
                .bind(None::<Vec<String>>)
                .bind(None::<String>)
                .bind(Vec::<String>::new()),
            Entry::Rule(rule) => q
                .bind(None::<Uuid>)
                .bind(&rule.name)
                .bind(rule.protection.as_str())
                .bind(rule.detector.kind.as_str())
                .bind(rule.detector.reference.as_deref())
                .bind(
                    rule.detector
                        .categories
                        .iter()
                        .map(|c| c.as_str().to_owned())
                        .collect::<Vec<_>>(),
                )
                .bind(Some(rule.input_action.as_str()))
                .bind(Some(rule.redact_scope.as_str()))
                .bind(Some(rule.output_mode.as_str()))
                .bind(rule.threshold.map(i32::from))
                .bind(Some(rule.on_error.as_str()))
                .bind(rule.timeout_ms.map(|t| t as i32))
                .bind(Some(
                    rule.workloads
                        .iter()
                        .map(|w| w.as_str().to_owned())
                        .collect::<Vec<_>>(),
                ))
                .bind(Some(rule.uninspectable.as_str()))
                .bind(&rule.detector.patterns),
        };
        q.execute(&mut **tx).await?;
    }
    Ok((id, revision))
}

/// Ids among `wanted` that belong to another layer's rules.
async fn foreign_ids(
    tx: &mut Transaction<'_, Postgres>,
    current: Option<&RuleSet>,
    wanted: &[Uuid],
) -> std::result::Result<HashSet<Uuid>, ApiError> {
    if wanted.is_empty() {
        return Ok(HashSet::new());
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM guardrail_rules WHERE id=ANY($1) AND rule_set_id IS DISTINCT FROM $2",
    )
    .bind(wanted)
    .bind(current.map(|s| s.id))
    .fetch_all(&mut **tx)
    .await?;
    Ok(ids.into_iter().collect())
}

/// Validate a PUT body against its layer.
async fn validated(
    tx: &mut Transaction<'_, Postgres>,
    body: LayerIn,
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
            compile: &patterns::valid,
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
        "enforced": false,
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
    let mut tx = platform_tx(&s, &u, true).await?;
    let scope = Scope::Type(&k);
    let current = load(&mut tx, scope).await?;
    let (local_only, rules) =
        validated(&mut tx, body, Layer::TypeDefault, None, current.as_ref()).await?;
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
    let mut tx = platform_tx(&s, &u, true).await?;
    platform(&mut tx, ws).await?;
    let scope = Scope::Override(ws);
    let current = load(&mut tx, scope).await?;
    let (local_only, rules) = validated(
        &mut tx,
        body,
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
