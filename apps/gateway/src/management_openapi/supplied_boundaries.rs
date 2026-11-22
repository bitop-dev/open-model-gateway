//! Whole-request counterfactuals for a supplied string boundary. Each variant
//! relaxes one keyword at one use site. The original document still resolves
//! every other reference, including a second reference to the same component.
use super::{Step, coverage, valid};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

fn conjunct(node: &mut Value, schema: Value) {
    node.as_object_mut()
        .unwrap()
        .entry("allOf")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .unwrap()
        .push(schema);
}

pub(super) fn relaxations(
    document: &Value,
    node: &Value,
    instance: &Value,
    path: &[Step],
    edge: (&str, &Value, Option<&Value>),
    depth: usize,
) -> Result<Vec<Value>> {
    ensure!(depth < 48, "supplied boundary schema nesting too deep");
    let mut out = Vec::new();
    let (keyword, bound, replacement) = edge;
    if path.is_empty() && node.get(keyword) == Some(bound) {
        let mut relaxed = node.clone();
        if let Some(value) = replacement {
            relaxed[keyword] = value.clone();
        } else {
            relaxed.as_object_mut().unwrap().remove(keyword);
        }
        out.push(relaxed);
    }

    let descend =
        |child, value, tail: &[Step]| relaxations(document, child, value, tail, edge, depth + 1);
    if let Some((target, _)) = coverage::follow(document, node)? {
        for variant in descend(target, instance, path)? {
            let mut local = node.clone();
            local.as_object_mut().unwrap().remove("$ref");
            conjunct(&mut local, variant);
            out.push(local);
        }
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        for (i, branch) in node[key].as_array().into_iter().flatten().enumerate() {
            if key == "allOf" || valid(document, branch, instance)? {
                for variant in descend(branch, instance, path)? {
                    let mut local = node.clone();
                    local[key][i] = variant;
                    out.push(local);
                }
            }
        }
    }
    if let Some(condition) = node.get("if") {
        let arm = if valid(document, condition, instance)? {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = node.get(arm) {
            for variant in descend(branch, instance, path)? {
                let mut local = node.clone();
                local[arm] = variant;
                out.push(local);
            }
        }
    }
    let Some((first, tail)) = path.split_first() else {
        return Ok(out);
    };
    match first {
        Step::Key(name) => {
            let mut selected = false;
            if let Some(child) = node["properties"].get(name) {
                selected = true;
                for variant in descend(child, &instance[name], tail)? {
                    let mut local = node.clone();
                    local["properties"][name] = variant;
                    out.push(local);
                }
            }
            for (pattern, child) in node["patternProperties"].as_object().into_iter().flatten() {
                let matcher = super::super::validation::pattern_validator(pattern)?;
                if !matcher.is_valid(&json!(name)) {
                    continue;
                }
                selected = true;
                for variant in descend(child, &instance[name], tail)? {
                    let mut local = node.clone();
                    // Specialize this map rule to the members of the actual
                    // request. Every other matching member keeps its own rule.
                    local["patternProperties"][pattern] = json!(true);
                    let mut members = serde_json::Map::new();
                    for other in instance.as_object().unwrap().keys() {
                        if matcher.is_valid(&json!(other)) {
                            members.insert(other.clone(), child.clone());
                        }
                    }
                    members.insert(name.clone(), variant);
                    conjunct(&mut local, json!({"properties":members}));
                    out.push(local);
                }
            }
            if !selected && let Some(child) = node.get("additionalProperties") {
                for variant in descend(child, &instance[name], tail)? {
                    let mut local = node.clone();
                    local["properties"][name] = variant;
                    out.push(local);
                }
            }
        }
        Step::Index(index) => {
            let prefix = node["prefixItems"].as_array().map_or(0, Vec::len);
            let child = node["prefixItems"].get(index).or_else(|| node.get("items"));
            if let Some(child) = child {
                for variant in descend(child, &instance[*index], tail)? {
                    let mut local = node.clone();
                    if *index >= prefix {
                        let entries = local
                            .as_object_mut()
                            .unwrap()
                            .entry("prefixItems")
                            .or_insert_with(|| json!([]))
                            .as_array_mut()
                            .unwrap();
                        entries.resize(*index + 1, child.clone());
                    }
                    local["prefixItems"][*index] = variant;
                    out.push(local);
                }
            }
        }
    }
    Ok(out)
}

/// Maximum Unicode scalar count, and the fewest UTF-8 bytes a match of
/// that width can occupy. A byte edge can stand in for the character edge
/// only when the byte budget makes the latter unreachable.
#[derive(Clone, Copy)]
pub(super) struct PatternWidth {
    pub(super) chars: usize,
    pub(super) min_bytes: usize,
    pub(super) bytes_per_char: usize,
}

pub(super) fn pattern_width(pattern: &str) -> Option<PatternWidth> {
    use regex_syntax::hir::{Hir, HirKind::*, Look};
    fn width(hir: &Hir) -> Option<(usize, usize)> {
        let (chars, min_bytes) = match hir.kind() {
            Empty | Look(_) => (0, 0),
            Literal(lit) => (
                std::str::from_utf8(&lit.0).ok()?.chars().count(),
                lit.0.len(),
            ),
            Class(class) => (1, class.minimum_len()?),
            Capture(c) => return width(&c.sub),
            Repetition(r) => {
                let (chars, bytes) = width(&r.sub)?;
                let n = r.max? as usize;
                (chars.checked_mul(n)?, bytes.checked_mul(n)?)
            }
            Concat(parts) => parts
                .iter()
                .try_fold((0usize, 0usize), |(chars, bytes), part| {
                    let (sub_chars, sub_bytes) = width(part)?;
                    Some((chars.checked_add(sub_chars)?, bytes.checked_add(sub_bytes)?))
                })?,
            Alternation(parts) => {
                return parts
                    .iter()
                    .map(width)
                    .collect::<Option<Vec<_>>>()?
                    .into_iter()
                    .max_by_key(|&(chars, bytes)| (chars, std::cmp::Reverse(bytes)));
            }
        };
        Some((chars, min_bytes))
    }
    fn min_scalar_bytes(hir: &Hir) -> Option<usize> {
        match hir.kind() {
            Literal(lit) => std::str::from_utf8(&lit.0)
                .ok()?
                .chars()
                .map(char::len_utf8)
                .min(),
            Class(class) => class.minimum_len(),
            kind => kind.subs().iter().filter_map(min_scalar_bytes).min(),
        }
    }
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let props = hir.properties();
    if !props.look_set_prefix().contains(Look::Start)
        || !props.look_set_suffix().contains(Look::End)
    {
        return None;
    }
    let (chars, min_bytes) = width(&hir)?;
    Some(PatternWidth {
        chars,
        min_bytes,
        bytes_per_char: min_scalar_bytes(&hir).unwrap_or(1),
    })
}

/// Widen a single variable-count range by one repetition. Source spans
/// preserve the repeated atom, surrounding alternatives, anchors and flags.
pub(super) fn wider_patterns(
    pattern: &str,
    width: usize,
    remaining_variant_bytes: &mut usize,
) -> Result<Vec<(String, usize)>> {
    use regex_syntax::ast::{Ast, RepetitionKind, RepetitionRange, parse::Parser};
    let ast = Parser::new().parse(pattern)?;
    let mut pending = vec![(&ast, false)];
    let mut wider = Vec::new();
    while let Some((node, nested)) = pending.pop() {
        match node {
            Ast::Group(group) => pending.push((&group.ast, nested)),
            Ast::Alternation(alt) => pending.extend(alt.asts.iter().map(|a| (a, nested))),
            Ast::Concat(seq) => pending.extend(seq.asts.iter().map(|a| (a, nested))),
            Ast::Repetition(rep) => {
                pending.push((&rep.ast, true));
                if let RepetitionKind::Range(RepetitionRange::Bounded(min, max)) = rep.op.kind
                    && min < max
                    && let Some(next) = max.checked_add(1)
                {
                    let span = &rep.op.span;
                    let suffix = if rep.greedy { "" } else { "?" };
                    let replacement = format!("{{{min},{next}}}{suffix}");
                    let range = span.start.offset..span.end.offset;
                    let size = pattern.len() - range.len() + replacement.len();
                    // Charge every attempted variant before allocating or parsing it.
                    *remaining_variant_bytes =
                        remaining_variant_bytes.checked_sub(size).ok_or_else(|| {
                            anyhow::anyhow!("supplied boundary regex derivation budget exceeded")
                        })?;
                    let mut variant = pattern.to_owned();
                    variant.replace_range(range, &replacement);
                    if let Some(new_width) = pattern_width(&variant)
                        && new_width.chars > width
                    {
                        // Inside another repetition the same source span may
                        // affect several runs. Only credit a single extra scalar there.
                        let step = if nested { 1 } else { new_width.chars - width };
                        wider.push((variant, step));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(wider)
}
