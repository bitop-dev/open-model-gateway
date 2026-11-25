//! Guardrail rule sets and rules in PostgreSQL (0092): reading a layer, a
//! workspace's platform layer, and replacing a layer's rules with a revision
//! bump. Stored values come from the tables' CHECK constraints; anything
//! unknown is a storage error, never a silently skipped rule.
use std::collections::HashSet;

use super::super::*;
use crate::guardrails::policy::{self, Entry, Layer, RuleSet, StoredRule};

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
pub(super) enum Scope<'a> {
    Type(&'a str),
    Override(Uuid),
    Local(Uuid),
}
impl Scope<'_> {
    pub(super) fn layer(self) -> Layer {
        match self {
            Self::Type(_) => Layer::TypeDefault,
            Self::Override(_) => Layer::WorkspaceOverride,
            Self::Local(_) => Layer::WorkspaceLocal,
        }
    }
}

pub(super) async fn load(
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
