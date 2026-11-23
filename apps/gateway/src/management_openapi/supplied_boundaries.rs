//! Whole-request counterfactuals for a supplied string boundary. Each variant
//! relaxes one keyword at one use site. The original document still resolves
//! every other reference, including a second reference to the same component.
use super::{Step, coverage, valid};
use anyhow::{Result, ensure};
use regex_syntax::hir::{Hir, HirKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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

/// Facts that hold for every choice at this instance path. The nodes chosen
/// by a fixture seed are useful for generating probes, but only a conjunction
/// can justify dropping one.
#[derive(Default)]
pub(super) struct ByteDominance {
    pub(super) cap: Option<u64>,
    pub(super) max_chars: Option<u64>,
    patterns: std::collections::BTreeSet<String>,
    pub(super) parent_choice: bool,
}

impl ByteDominance {
    pub(super) fn at(
        document: &Value,
        root: &Value,
        path: &[Step],
        nodes: &[&Value],
    ) -> Result<Self> {
        let mut rules = Self::default();
        if nodes
            .iter()
            .any(|node| node.get(super::MAX_BYTES).is_some())
        {
            let mut remaining = 4096;
            rules.collect(document, root, path, &mut remaining, 0)?;
        }
        Ok(rules)
    }

    fn collect(
        &mut self,
        document: &Value,
        schema: &Value,
        path: &[Step],
        remaining: &mut usize,
        depth: usize,
    ) -> Result<()> {
        // Unvisited constraints cannot contribute to the proof. A projection
        // of unknown ancestry still needs whole-body verdicts.
        if *remaining == 0 || depth >= 48 {
            self.parent_choice |= !path.is_empty();
            return Ok(());
        }
        *remaining -= 1;
        for node in super::chain(document, schema)? {
            if let Some((first, tail)) = path.split_first() {
                self.parent_choice |= node.get("if").is_some();
                // Changing a member or element preserves its container type.
                // The sole arm compatible with that type is a conjunction for
                // this substitution (e.g. object versus positional wire forms).
                let container = match first {
                    Step::Key(_) => "object",
                    Step::Index(_) => "array",
                };
                for key in ["anyOf", "oneOf"] {
                    let Some(arms) = node[key].as_array() else {
                        continue;
                    };
                    if arms.len() > *remaining {
                        self.parent_choice = true;
                        continue;
                    }
                    *remaining -= arms.len();
                    let mut possible = arms.iter().filter(|arm| match &arm["type"] {
                        Value::String(t) => t == container,
                        Value::Array(types) => types.iter().any(|t| t == container),
                        _ => arm.as_bool() != Some(false),
                    });
                    if let Some(only) = possible.next() {
                        if possible.next().is_none() {
                            self.collect(document, only, path, remaining, depth + 1)?;
                        } else {
                            self.parent_choice = true;
                        }
                    }
                }
                let child = match first {
                    Step::Key(name) => super::member_schema(&[node], name)?,
                    Step::Index(index) => super::element_schema(&[node], *index),
                };
                if let Some(child) = child {
                    self.collect(document, &child, tail, remaining, depth + 1)?;
                }
            } else {
                if let Some(cap) = node[super::MAX_BYTES].as_u64() {
                    self.cap = Some(self.cap.map_or(cap, |previous| previous.min(cap)));
                }
                if let Some(max) = node["maxLength"].as_u64() {
                    self.max_chars = Some(self.max_chars.map_or(max, |previous| previous.min(max)));
                }
                if let Some(pattern) = node["pattern"].as_str() {
                    self.patterns.insert(pattern.to_owned());
                }
            }
            for conjunct in node["allOf"].as_array().into_iter().flatten() {
                self.collect(document, conjunct, path, remaining, depth + 1)?;
            }
        }
        Ok(())
    }

    pub(super) fn excludes(&self, width: u64) -> bool {
        let Some(cap) = self.cap else {
            return false;
        };
        if cap < width {
            return true;
        }
        let widths: Vec<_> = self
            .patterns
            .iter()
            .filter_map(|p| pattern_width(p))
            .collect();
        byte_cap_prevents_width(widths.iter(), width, cap)
    }
}

/// Maximum Unicode scalar count, and the fewest UTF-8 bytes a match of
/// that width can occupy. A byte edge can stand in for the character edge
/// only when the byte budget makes the latter unreachable.
pub(super) struct PatternWidth {
    pub(super) chars: usize,
    min_bytes: usize,
    bytes_per_char: usize,
    hir: Hir,
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
        hir,
    })
}

// Each entry is the least UTF-8 cost for its scalar count. The last
// bucket includes longer matches: these could still cross maxLength.
type Costs = BTreeMap<usize, usize>;

struct WidthBudget {
    target: usize,
    remaining: usize,
}

impl WidthBudget {
    fn charge(&mut self, steps: usize) -> Option<()> {
        self.remaining = self.remaining.checked_sub(steps)?;
        Some(())
    }

    fn concat(&mut self, left: &Costs, right: &Costs) -> Option<Costs> {
        self.charge(left.len().checked_mul(right.len())?)?;
        let mut costs = Costs::new();
        for (&a, &a_bytes) in left {
            for (&b, &b_bytes) in right {
                let bytes = a_bytes.saturating_add(b_bytes);
                let best = costs.entry((a + b).min(self.target)).or_insert(bytes);
                *best = (*best).min(bytes);
            }
        }
        Some(costs)
    }

    // Binary powers keep large repeat counts logarithmic. Adding an empty
    // choice to an atom turns an exact power into up to that many copies.
    fn power(&mut self, mut unit: Costs, mut count: u32) -> Option<Costs> {
        let mut result = Costs::from([(0, 0)]);
        while count > 0 {
            if count % 2 == 1 {
                result = self.concat(&result, &unit)?;
            }
            count /= 2;
            if count > 0 {
                unit = self.concat(&unit, &unit)?;
            }
        }
        Some(result)
    }

    fn costs(&mut self, hir: &Hir) -> Option<Costs> {
        self.charge(1)?;
        match hir.kind() {
            HirKind::Empty | HirKind::Look(_) => Some(Costs::from([(0, 0)])),
            HirKind::Literal(lit) => {
                let chars = std::str::from_utf8(&lit.0).ok()?.chars().count();
                Some(Costs::from([(chars.min(self.target), lit.0.len())]))
            }
            HirKind::Class(class) => Some(Costs::from([(1, class.minimum_len()?)])),
            HirKind::Capture(c) => self.costs(&c.sub),
            HirKind::Concat(parts) => {
                let mut result = Costs::from([(0, 0)]);
                for part in parts {
                    let next = self.costs(part)?;
                    result = self.concat(&result, &next)?;
                }
                Some(result)
            }
            HirKind::Alternation(parts) => {
                let mut result = Costs::new();
                for part in parts {
                    let next = self.costs(part)?;
                    self.charge(next.len())?;
                    for (chars, bytes) in next {
                        let best = result.entry(chars).or_insert(bytes);
                        *best = (*best).min(bytes);
                    }
                }
                Some(result)
            }
            HirKind::Repetition(r) => {
                let mut unit = self.costs(&r.sub)?;
                let required = self.power(unit.clone(), r.min)?;
                unit.insert(0, 0);
                let optional = self.power(unit, r.max? - r.min)?;
                self.concat(&required, &optional)
            }
        }
    }
}

/// Prove the byte cap excludes all matches at or above the character edge.
/// Cheap bounds apply at any width. The cost table is limited in size and
/// cumulative work across the patterns at this edge. An inconclusive result
/// leaves the character witness required.
pub(super) fn byte_cap_prevents_width<'a>(
    patterns: impl Iterator<Item = &'a PatternWidth>,
    target: u64,
    cap: u64,
) -> bool {
    let mut budget = WidthBudget {
        target: target as usize,
        remaining: 64 * 1024,
    };
    for pattern in patterns {
        if cap / (pattern.bytes_per_char as u64) < target {
            return true;
        }
        if target > pattern.chars as u64 || cap >= pattern.min_bytes as u64 {
            continue;
        }
        if target == pattern.chars as u64 {
            return true;
        }
        if (1..=256).contains(&target)
            && let Some(costs) = budget.costs(&pattern.hir)
            && costs
                .get(&budget.target)
                .is_some_and(|&bytes| bytes as u64 > cap)
        {
            return true;
        }
    }
    false
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

#[test]
fn intermediate_width_costs_combine_branches_and_repetitions() {
    for (pattern, target, least_bytes) in [
        ("^(a|é{2,8})$", 6, 12),
        ("^([aé]|[aé]{2,8})$", 6, 6),
        ("^(?:a|é{2,3})(?:b|✓{2,3})$", 5, 12),
        ("^(?:a|é{2,3}){2}$", 5, 10),
        ("^(?:é{6}|a{7}|✓{9})$", 6, 7),
    ] {
        let width = pattern_width(pattern).unwrap();
        assert!(
            byte_cap_prevents_width([&width].into_iter(), target, least_bytes - 1),
            "{pattern}"
        );
        assert!(
            !byte_cap_prevents_width([&width].into_iter(), target, least_bytes),
            "{pattern}"
        );
    }
}

#[test]
fn intermediate_width_inconclusive_costs_leave_the_edge_required() {
    let width = pattern_width("^(a|é{2,1000000})$").unwrap();
    // The cheap ASCII alternative prevents the general scalar bound from
    // deciding this large intermediate width. No width-sized table is built.
    assert!(!byte_cap_prevents_width(
        [&width].into_iter(),
        500000,
        750000
    ));
    assert!(!byte_cap_prevents_width([&width].into_iter(), 6, u64::MAX));
    let mut small_budget = WidthBudget {
        target: 6,
        remaining: 1,
    };
    assert!(small_budget.costs(&width.hir).is_none());
}
