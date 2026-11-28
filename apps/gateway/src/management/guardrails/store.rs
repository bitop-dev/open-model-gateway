//! Guardrail rule sets and rules in PostgreSQL (0092): reading a layer, a
//! workspace's platform layer, and replacing a layer's rules with a revision
//! bump. Stored values come from the tables' CHECK constraints; anything
//! unknown is a storage error, never a silently skipped rule.
use std::collections::HashSet;

use super::super::*;
use crate::guardrails::policy::{Entry, RuleSet, StoredRule};
pub(super) use crate::guardrails::stored::Scope;
use crate::guardrails::stored::{self, Unreadable};

impl From<Unreadable> for ApiError {
    fn from(_: Unreadable) -> Self {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored guardrail rule is not readable",
        )
    }
}

pub(super) async fn load(
    tx: &mut Transaction<'_, Postgres>,
    scope: Scope<'_>,
) -> std::result::Result<Option<RuleSet>, ApiError> {
    Ok(stored::load(tx, scope).await??)
}

/// A workspace's kind and its platform layer: the override if one exists,
/// else its kind's type default.
pub(super) struct Platform {
    pub(super) kind: String,
    pub(super) type_default: Option<RuleSet>,
    pub(super) workspace_override: Option<RuleSet>,
}
impl Platform {
    pub(super) fn current(&self) -> Option<&RuleSet> {
        self.workspace_override
            .as_ref()
            .or(self.type_default.as_ref())
    }
}
pub(super) async fn platform(
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
pub(super) async fn store(
    tx: &mut Transaction<'_, Postgres>,
    scope: Scope<'_>,
    current: Option<&RuleSet>,
    local_only: Option<bool>,
    rules: &[StoredRule],
    actor: Uuid,
) -> std::result::Result<(Uuid, i64), ApiError> {
    let (id, revision) = match current {
        Some(set) => {
            // The revision moves first: the database accepts rule changes
            // only in the transaction that moved it (0092).
            sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,local_only=$2,updated_by=$3 WHERE id=$1")
                .bind(set.id)
                .bind(local_only)
                .bind(actor)
                .execute(&mut **tx)
                .await?;
            sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
                .bind(set.id)
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
        let q = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,name,protection,detector_kind,detector_ref,categories,input_action,redact_scope,output_mode,threshold,on_error,timeout_ms,workloads,uninspectable,patterns,kinds) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13::numeric/1000,$14,$15,$16,$17,$18,$19)")
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
                .bind(Vec::<String>::new())
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
                .bind(&rule.detector.patterns)
                .bind(
                    rule.detector
                        .kinds
                        .iter()
                        .map(|k| k.as_str().to_owned())
                        .collect::<Vec<_>>(),
                ),
        };
        q.execute(&mut **tx).await?;
    }
    Ok((id, revision))
}

/// Ids among `wanted` that belong to another layer's rules.
pub(super) async fn foreign_ids(
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
