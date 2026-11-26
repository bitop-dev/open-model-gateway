//! Reading stored guardrail rule sets (0092), shared by the management routes
//! and the engine's policy cache. Stored values come from the tables' CHECK
//! constraints; one this release does not know is `Unreadable`, never a
//! silently skipped rule.
use sqlx::PgConnection;
use uuid::Uuid;

use super::policy::{Layer, RuleSet, StoredRule};

/// A stored value this release cannot read.
#[derive(Clone, Copy, Debug)]
pub struct Unreadable;

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
fn decode(row: RuleRow) -> Result<StoredRule, Unreadable> {
    use super::policy::*;
    let bad = || Unreadable;
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
pub enum Scope<'a> {
    Type(&'a str),
    Override(Uuid),
    Local(Uuid),
}
impl Scope<'_> {
    pub fn layer(self) -> Layer {
        match self {
            Self::Type(_) => Layer::TypeDefault,
            Self::Override(_) => Layer::WorkspaceOverride,
            Self::Local(_) => Layer::WorkspaceLocal,
        }
    }
}

/// One rule set and its rules, in order; `Ok(Err(Unreadable))` for a stored
/// value this release does not know.
pub async fn load(
    conn: &mut PgConnection,
    scope: Scope<'_>,
) -> Result<Result<Option<RuleSet>, Unreadable>, sqlx::Error> {
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
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, revision, local_only)) = head else {
        return Ok(Ok(None));
    };
    let rows: Vec<RuleRow> = sqlx::query_as(&format!(
        "SELECT {RULE_COLUMNS} FROM guardrail_rules WHERE rule_set_id=$1 ORDER BY position"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let rules = match rows.into_iter().map(decode).collect::<Result<_, _>>() {
        Ok(rules) => rules,
        Err(e) => return Ok(Err(e)),
    };
    Ok(Ok(Some(RuleSet {
        id,
        layer: scope.layer(),
        revision,
        local_only,
        rules,
    })))
}

/// A workspace's stored layers: its platform layer (the override if one
/// exists, else its kind's type default) and its local layer. One query when
/// none exists (the common case), else one more per layer for its rules.
pub async fn workspace_layers(
    conn: &mut PgConnection,
    workspace: Uuid,
) -> Result<Result<(Option<RuleSet>, Option<RuleSet>), Unreadable>, sqlx::Error> {
    let heads = workspace_heads(conn, workspace).await?;
    layers_of(conn, heads).await
}

/// The rule sets that apply to a workspace: id, layer, revision and
/// `local_only` (one statement). Every change of a set or its rules moves
/// its revision (0092), and a kind change selects another type default, so
/// equal heads mean an equal policy.
pub type Heads = Vec<(Uuid, String, i64, Option<bool>)>;
pub async fn workspace_heads(
    conn: &mut PgConnection,
    workspace: Uuid,
) -> Result<Heads, sqlx::Error> {
    let mut heads: Heads = sqlx::query_as(
        "SELECT s.id,s.layer,s.revision,s.local_only FROM guardrail_rule_sets s JOIN workspaces w ON w.id=$1
         WHERE (s.layer='type_default' AND s.kind=w.kind) OR s.workspace_id=$1",
    )
    .bind(workspace)
    .fetch_all(&mut *conn)
    .await?;
    heads.sort();
    Ok(heads)
}

/// The layers of `heads`, with their rules (one statement per set).
pub async fn layers_of(
    conn: &mut PgConnection,
    heads: Heads,
) -> Result<Result<(Option<RuleSet>, Option<RuleSet>), Unreadable>, sqlx::Error> {
    if heads.is_empty() {
        return Ok(Ok((None, None)));
    }
    let mut sets: Vec<RuleSet> = Vec::with_capacity(heads.len());
    for (id, layer, revision, local_only) in heads {
        let Some(layer) = Layer::parse(&layer) else {
            return Ok(Err(Unreadable));
        };
        let rows: Vec<RuleRow> = sqlx::query_as(&format!(
            "SELECT {RULE_COLUMNS} FROM guardrail_rules WHERE rule_set_id=$1 ORDER BY position"
        ))
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        let rules = match rows.into_iter().map(decode).collect::<Result<_, _>>() {
            Ok(rules) => rules,
            Err(e) => return Ok(Err(e)),
        };
        sets.push(RuleSet {
            id,
            layer,
            revision,
            local_only,
            rules,
        });
    }
    let take = |sets: &mut Vec<RuleSet>, layer: Layer| {
        sets.iter()
            .position(|s| s.layer == layer)
            .map(|i| sets.swap_remove(i))
    };
    let local = take(&mut sets, Layer::WorkspaceLocal);
    let over = take(&mut sets, Layer::WorkspaceOverride);
    let default = take(&mut sets, Layer::TypeDefault);
    Ok(Ok((over.or(default), local)))
}
