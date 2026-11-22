//! Which DECLARED request locations did the vector runs of an operation reach?
//!
//! The vector walk only mutates what is PRESENT in the valid body (or a seed):
//! an optional property the base omits, a map value, a positional index or a
//! `oneOf` arm the base does not match is silently never probed. This module
//! names those locations so `finish` can fail on them.
//!
//! A location is identified by its SCHEMA path from the request schema root
//! (references are transparent): `/properties/reason`, `/oneOf/1/prefixItems/2`,
//! `/properties/details/additionalProperties`, `/items`. Declared locations are:
//! every property, every `patternProperties` / schema-valued
//! `additionalProperties` value, every `prefixItems` index and the `items` tail
//! and every `oneOf` / `anyOf` arm. `allOf` members and `then`/`else` contents are
//! walked for their contents. Scalar-constraining conditional arms and array
//! bounds are obligations in their own right. A location is REACHED by a base body when the body
//! carries it (and, for an arm, matches the arm).
//!
//! Exempt only with `x-omg-unprobed-location: "<reason>"` on the location's
//! schema (property, map value, element or arm); the contents of an exempt
//! location are exempt too. Messages name schema paths, never instance values.
use super::coverage::follow;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const UNPROBED: &str = "x-omg-unprobed-location";

pub(super) struct Declared {
    pub(super) kind: &'static str,
    pub(super) exempt: bool,
    /// The location's identity within the operation: the component it lives in
    /// (the innermost `$ref` it is reached through) plus its path inside that
    /// component, with `allOf`/`then`/`else` segments dropped. One shared
    /// definition reached through several paths (a `then` branch restating a
    /// property, a second reference) has one identity.
    pub(super) identity: String,
}

/// Where identities are measured from: the innermost referenced component and
/// the length of the path at which it was entered.
type Scope<'a> = Option<(&'a str, usize)>;

fn identity(key: &str, scope: Scope) -> String {
    let (base, relative) = match scope {
        Some((component, at)) => (component, &key[at..]),
        None => ("", key),
    };
    let mut out = String::from(base);
    let mut parts = relative.split('/').skip(1).peekable();
    while let Some(part) = parts.next() {
        match part {
            "allOf"
                if parts
                    .peek()
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) =>
            {
                parts.next();
            }
            "then" | "else" => {}
            _ => {
                out.push('/');
                out.push_str(part);
            }
        }
    }
    out
}

fn segment(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// A reviewed reason on the schema or on any schema it references.
fn exempt(document: &Value, node: &Value) -> bool {
    let mut node = node;
    for _ in 0..16 {
        if node
            .get(UNPROBED)
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.trim().is_empty())
        {
            return true;
        }
        match follow(document, node) {
            Ok(Some((target, _))) => node = target,
            _ => return false,
        }
    }
    false
}

/// Each dependency needs a valid base carrying both properties. The walk
/// sends that base and an omission of the dependent while the trigger remains.
pub(super) fn dependencies(node: &Value) -> impl Iterator<Item = (&str, &str)> {
    node["dependentRequired"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(trigger, names)| {
            names
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(move |name| (trigger.as_str(), name))
        })
}

fn dependency_key(key: &str, trigger: &str, name: &str) -> String {
    format!(
        "{key}/dependentRequired/{}/{}",
        segment(trigger),
        segment(name)
    )
}

fn pattern_matches(pattern: &str, name: &str) -> Result<bool> {
    Ok(super::validation::pattern_validator(pattern)?.is_valid(&Value::String(name.to_owned())))
}

#[allow(clippy::too_many_arguments)]
fn declare_child(
    document: &Value,
    child: &Value,
    key: String,
    kind: &'static str,
    out: &mut BTreeMap<String, Declared>,
    stack: &mut Vec<String>,
    depth: usize,
    scope: Scope,
) -> Result<()> {
    if !child.is_object() {
        return Ok(());
    }
    let excused = exempt(document, child);
    // One data location may be declared several times (a `$ref` target and the
    // keywords beside it, `allOf` members): it is exempt only if EVERY
    // declaration is, never by whichever was written last.
    out.entry(key.clone())
        .and_modify(|declared| declared.exempt &= excused)
        .or_insert(Declared {
            kind,
            exempt: excused,
            identity: identity(&key, scope),
        });
    // The contents are declared too: an exemption excuses a location that is
    // never carried, not the contents of one that is (see `unreached`).
    declare(document, child, &key, out, stack, depth + 1, scope)
}

/// Scalar constraints have no child location that could otherwise witness the arm.
fn scalar_constraints(document: &Value, node: &Value, depth: usize) -> Result<bool> {
    ensure!(depth < 48, "conditional schema nesting too deep");
    if [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "enum",
        "const",
        "x-omg-max-bytes",
    ]
    .iter()
    .any(|k| node.get(*k).is_some())
        || node["type"]
            .as_str()
            .into_iter()
            .chain(
                node["type"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            )
            .any(|t| matches!(t, "integer" | "number" | "boolean" | "string" | "null"))
    {
        return Ok(true);
    }
    for branch in node["allOf"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(follow(document, node)?.map(|(target, _)| target))
    {
        if scalar_constraints(document, branch, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn declare(
    document: &Value,
    node: &Value,
    key: &str,
    out: &mut BTreeMap<String, Declared>,
    stack: &mut Vec<String>,
    depth: usize,
    scope: Scope,
) -> Result<()> {
    ensure!(depth < 48, "request schema nesting too deep");
    if let Some((target, pointer)) = follow(document, node)?
        && !stack.contains(&pointer)
    {
        stack.push(pointer.clone());
        let entered = Some((pointer.as_str(), key.len()));
        let declared = declare(document, target, key, out, stack, depth + 1, entered);
        stack.pop();
        declared?;
    }
    for bound in ["minItems", "maxItems"] {
        if node.get(bound).is_some() {
            let key = format!("{key}/{bound}");
            out.insert(
                key.clone(),
                Declared {
                    kind: "array bound",
                    exempt: false,
                    // Keep conditional arms within the defining component distinct.
                    // Another reference to that same bound shares its evidence.
                    identity: match scope {
                        Some((component, start)) => format!("{component}{}", &key[start..]),
                        None => key,
                    },
                },
            );
        }
    }
    for (trigger, name) in dependencies(node) {
        let key = dependency_key(key, trigger, name);
        out.insert(
            key.clone(),
            Declared {
                kind: "dependent property",
                exempt: false,
                identity: identity(&key, scope),
            },
        );
    }
    for (name, child) in node["properties"].as_object().into_iter().flatten() {
        let child_key = format!("{key}/properties/{}", segment(name));
        declare_child(
            document, child, child_key, "property", out, stack, depth, scope,
        )?;
    }
    for (pattern, child) in node["patternProperties"].as_object().into_iter().flatten() {
        let child_key = format!("{key}/patternProperties/{}", segment(pattern));
        declare_child(
            document,
            child,
            child_key,
            "map value",
            out,
            stack,
            depth,
            scope,
        )?;
    }
    if let Some(child) = node.get("additionalProperties") {
        declare_child(
            document,
            child,
            format!("{key}/additionalProperties"),
            "map value",
            out,
            stack,
            depth,
            scope,
        )?;
    }
    for (i, child) in node["prefixItems"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let child_key = format!("{key}/prefixItems/{i}");
        declare_child(
            document,
            child,
            child_key,
            "array element",
            out,
            stack,
            depth,
            scope,
        )?;
    }
    if let Some(child) = node.get("items") {
        declare_child(
            document,
            child,
            format!("{key}/items"),
            "array element",
            out,
            stack,
            depth,
            scope,
        )?;
    }
    for keyword in ["oneOf", "anyOf"] {
        for (i, child) in node[keyword].as_array().into_iter().flatten().enumerate() {
            let child_key = format!("{key}/{keyword}/{i}");
            declare_child(
                document,
                child,
                child_key,
                "union arm",
                out,
                stack,
                depth,
                scope,
            )?;
        }
    }
    for (i, child) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        declare(
            document,
            child,
            &format!("{key}/allOf/{i}"),
            out,
            stack,
            depth + 1,
            scope,
        )?;
    }
    for keyword in ["then", "else"] {
        if let Some(child) = node.get(keyword).filter(|c| c.is_object()) {
            if node.get("if").is_some() && scalar_constraints(document, child, 0)? {
                let path = format!("{key}/{keyword}");
                out.insert(
                    path.clone(),
                    Declared {
                        kind: "conditional arm",
                        exempt: exempt(document, child),
                        identity: path,
                    },
                );
            }
            declare(
                document,
                child,
                &format!("{key}/{keyword}"),
                out,
                stack,
                depth + 1,
                scope,
            )?;
        }
    }
    Ok(())
}

/// Every declared request location of a request schema, by schema path.
pub(super) fn declared(document: &Value, schema: &Value) -> Result<BTreeMap<String, Declared>> {
    let mut out = BTreeMap::new();
    declare(document, schema, "", &mut out, &mut Vec::new(), 0, None)?;
    Ok(out)
}

/// What a reach computation may claim: with `walked`, only the instance
/// locations (JSON pointers, `""` for the root) a vector walk really mutated.
struct Ctx<'a> {
    document: &'a Value,
    walked: Option<&'a BTreeSet<String>>,
}
impl Ctx<'_> {
    fn counts(&self, at: &str) -> bool {
        self.walked.is_none_or(|walked| walked.contains(at))
    }
}

#[allow(clippy::too_many_arguments)]
fn visit(
    ctx: &Ctx,
    child: &Value,
    key: String,
    instance: &Value,
    at: &str,
    out: &mut BTreeSet<String>,
    stack: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<()> {
    if !child.is_object() || !ctx.counts(at) {
        return Ok(());
    }
    out.insert(key.clone());
    reach(ctx, child, instance, &key, at, out, stack, depth + 1)
}

#[allow(clippy::too_many_arguments)]
fn reach(
    ctx: &Ctx,
    node: &Value,
    instance: &Value,
    key: &str,
    at: &str,
    out: &mut BTreeSet<String>,
    stack: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<()> {
    let document = ctx.document;
    ensure!(depth < 48, "request body nesting too deep");
    if let Some((target, pointer)) = follow(document, node)? {
        let frame = (pointer, key.to_owned());
        if !stack.contains(&frame) {
            stack.push(frame);
            let walked = reach(ctx, target, instance, key, at, out, stack, depth + 1);
            stack.pop();
            walked?;
        }
    }
    let matches = |schema: &Value| -> Result<bool> {
        Ok(super::responses::validator(document, schema)?.is_valid(instance))
    };
    for keyword in ["oneOf", "anyOf"] {
        for (i, arm) in node[keyword].as_array().into_iter().flatten().enumerate() {
            if arm.is_object() && matches(arm)? {
                visit(
                    ctx,
                    arm,
                    format!("{key}/{keyword}/{i}"),
                    instance,
                    at,
                    out,
                    stack,
                    depth,
                )?;
            }
        }
    }
    for (i, arm) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        reach(
            ctx,
            arm,
            instance,
            &format!("{key}/allOf/{i}"),
            at,
            out,
            stack,
            depth + 1,
        )?;
    }
    if let Some(condition) = node.get("if") {
        let keyword = if matches(condition)? { "then" } else { "else" };
        if let Some(branch) = node.get(keyword).filter(|b| b.is_object()) {
            if ctx.counts(at) {
                out.insert(format!("{key}/{keyword}"));
            }
            reach(
                ctx,
                branch,
                instance,
                &format!("{key}/{keyword}"),
                at,
                out,
                stack,
                depth + 1,
            )?;
        }
    }
    if let Some(object) = instance.as_object() {
        for (trigger, name) in dependencies(node) {
            if object.contains_key(trigger)
                && object.contains_key(name)
                && ctx.counts(&format!("{at}/{}", segment(name)))
            {
                out.insert(dependency_key(key, trigger, name));
            }
        }
        let properties = node["properties"].as_object();
        let patterns: Vec<(&String, &Value)> = node["patternProperties"]
            .as_object()
            .into_iter()
            .flatten()
            .collect();
        for (name, value) in object {
            let child_at = format!("{at}/{}", segment(name));
            if let Some(child) = properties.and_then(|p| p.get(name)) {
                visit(
                    ctx,
                    child,
                    format!("{key}/properties/{}", segment(name)),
                    value,
                    &child_at,
                    out,
                    stack,
                    depth,
                )?;
            }
            let mut selected = properties.is_some_and(|p| p.contains_key(name));
            for (pattern, child) in &patterns {
                if pattern_matches(pattern, name)? {
                    selected = true;
                    visit(
                        ctx,
                        child,
                        format!("{key}/patternProperties/{}", segment(pattern)),
                        value,
                        &child_at,
                        out,
                        stack,
                        depth,
                    )?;
                }
            }
            if !selected && let Some(child) = node.get("additionalProperties") {
                visit(
                    ctx,
                    child,
                    format!("{key}/additionalProperties"),
                    value,
                    &child_at,
                    out,
                    stack,
                    depth,
                )?;
            }
        }
    }
    if let Some(array) = instance.as_array() {
        if ctx.counts(at) {
            for bound in ["minItems", "maxItems"] {
                if node.get(bound).is_some() {
                    out.insert(format!("{key}/{bound}"));
                }
            }
        }
        let prefix = node["prefixItems"].as_array().map_or(0, Vec::len);
        for (i, child) in node["prefixItems"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(value) = array.get(i) {
                visit(
                    ctx,
                    child,
                    format!("{key}/prefixItems/{i}"),
                    value,
                    &format!("{at}/{i}"),
                    out,
                    stack,
                    depth,
                )?;
            }
        }
        if let Some(child) = node.get("items") {
            for (i, value) in array.iter().enumerate().skip(prefix) {
                visit(
                    ctx,
                    child,
                    format!("{key}/items"),
                    value,
                    &format!("{at}/{i}"),
                    out,
                    stack,
                    depth,
                )?;
            }
        }
    }
    Ok(())
}

/// The declared locations a base body carries (and the arms it matches).
pub(super) fn reached(
    document: &Value,
    schema: &Value,
    instance: &Value,
) -> Result<BTreeSet<String>> {
    let ctx = Ctx {
        document,
        walked: None,
    };
    let mut out = BTreeSet::new();
    reach(&ctx, schema, instance, "", "", &mut out, &mut Vec::new(), 0)?;
    Ok(out)
}

/// The declared locations of `instance` that a vector walk really MUTATED:
/// `walked` are the JSON pointers (`""` for the root) the walk generated
/// vectors for. A location the body merely contains is not reached.
pub(super) fn reached_by_walk(
    document: &Value,
    schema: &Value,
    instance: &Value,
    walked: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let ctx = Ctx {
        document,
        walked: Some(walked),
    };
    let mut out = BTreeSet::new();
    reach(&ctx, schema, instance, "", "", &mut out, &mut Vec::new(), 0)?;
    Ok(out)
}

/// A location is excused when it, or an ancestor, carries a reviewed reason AND
/// that exempt location was not itself reached: the exemption covers an absent
/// subtree, never unreached children of a location the bodies do carry.
fn excused(key: &str, declared: &BTreeMap<String, Declared>, reached: &BTreeSet<String>) -> bool {
    declared.iter().any(|(owner, location)| {
        location.exempt
            && !reached.contains(owner)
            && (owner == key || key.starts_with(&format!("{owner}/")))
    })
}

/// `(schema path, kind)` of every declared location never reached and not excused.
pub(super) fn unreached(
    document: &Value,
    schema: &Value,
    reached: &BTreeSet<String>,
) -> Result<Vec<(String, &'static str)>> {
    let declared = declared(document, schema)?;
    // A shared definition reached through any path of the operation counts on
    // every path to it (see [`Declared::identity`]); union arms stay distinct.
    let reached_identities: BTreeSet<&str> = reached
        .iter()
        .filter_map(|key| declared.get(key).map(|d| d.identity.as_str()))
        .collect();
    Ok(declared
        .iter()
        .filter(|(key, location)| {
            !reached.contains(*key)
                && !reached_identities.contains(location.identity.as_str())
                && !excused(key, &declared, reached)
        })
        .map(|(key, location)| (key.clone(), location.kind))
        .collect())
}

/// Schema paths of every exemption that is STALE: the location carries a reviewed
/// [`UNPROBED`] reason yet some vector run reached it (nested ones included).
pub(super) fn stale(
    document: &Value,
    schema: &Value,
    reached: &BTreeSet<String>,
) -> Result<Vec<String>> {
    Ok(declared(document, schema)?
        .into_iter()
        .filter(|(key, location)| location.exempt && reached.contains(key))
        .map(|(key, _)| key)
        .collect())
}

/// How a declared location can be reached: seeds add absent properties and the
/// next positional item, nothing else (tail items, map values and union arms
/// need a valid body that carries or matches them).
pub(super) fn how_to_reach(key: &str, kind: &str) -> &'static str {
    let positional = key
        .rsplit_once("/prefixItems/")
        .is_some_and(|(_, index)| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()));
    if kind == "property" || positional {
        "carry it in a valid body or a seed"
    } else {
        "carry it in a valid body (a seed cannot add it)"
    }
}
