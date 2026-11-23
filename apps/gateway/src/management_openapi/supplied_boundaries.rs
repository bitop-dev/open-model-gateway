//! Whole-request counterfactuals for a supplied string boundary. Each variant
//! relaxes one keyword at one use site. The original document still resolves
//! every other reference, including a second reference to the same component.
use super::{Step, accepts, coverage};
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

// The instance path is fixed during a check. Schema keywords and their child
// indices identify its use sites even when two arms reference the same pattern.
pub(super) type SchemaStep = (&'static str, usize);

pub(super) struct Edge<'a> {
    pub(super) keyword: &'a str,
    pub(super) bound: &'a Value,
    pub(super) replacement: Option<&'a Value>,
    pub(super) site: Option<&'a [SchemaStep]>,
}

pub(super) struct PatternUse {
    pub(super) annotated: bool,
    pub(super) site: Vec<SchemaStep>,
}

pub(super) struct PatternSource<'a> {
    pub(super) document: &'a Value,
    pub(super) schema: &'a Value,
    pub(super) seed: &'a Value,
    pub(super) path: &'a [Step],
}

impl<'a> PatternSource<'a> {
    pub(super) fn uses(&self) -> Result<BTreeMap<&'a str, Vec<PatternUse>>> {
        let mut uses = BTreeMap::new();
        transform(
            self.document,
            self.schema,
            self.seed,
            self.path,
            &mut Vec::new(),
            false,
            &mut |node, annotated, site| {
                if let Some(pattern) = node["pattern"].as_str() {
                    uses.entry(pattern)
                        .or_insert_with(Vec::new)
                        .push(PatternUse {
                            annotated,
                            site: site.to_vec(),
                        });
                }
                None
            },
        )?;
        Ok(uses)
    }

    pub(super) fn relaxations(&self, instance: &Value, edge: Edge<'_>) -> Result<Vec<Value>> {
        transform(
            self.document,
            self.schema,
            instance,
            self.path,
            &mut Vec::new(),
            false,
            &mut |node, _, site| {
                if node.get(edge.keyword) != Some(edge.bound)
                    || edge.site.is_some_and(|owner| owner != site)
                {
                    return None;
                }
                let mut relaxed = node.clone();
                if let Some(value) = edge.replacement {
                    relaxed[edge.keyword] = value.clone();
                } else {
                    relaxed.as_object_mut().unwrap().remove(edge.keyword);
                }
                Some(relaxed)
            },
        )
    }
}

/// Visit the active use sites at the requested instance path. Rebuild a
/// whole-schema counterfactual only for a use the visitor changes.
fn transform<'a>(
    document: &'a Value,
    node: &'a Value,
    instance: &Value,
    path: &[Step],
    site: &mut Vec<SchemaStep>,
    inherited_annotation: bool,
    visit: &mut impl FnMut(&'a Value, bool, &[SchemaStep]) -> Option<Value>,
) -> Result<Vec<Value>> {
    ensure!(site.len() < 48, "supplied boundary schema nesting too deep");
    let mut out = Vec::new();
    let annotated = inherited_annotation || node.get(super::PATTERN_BOUNDARIES).is_some();
    if path.is_empty()
        && let Some(relaxed) = visit(node, annotated, site)
    {
        out.push(relaxed);
    }
    let mut descend = |step, child: &'a Value, value: &Value, tail: &[Step], carry| {
        site.push(step);
        let result = transform(document, child, value, tail, site, carry, visit);
        site.pop();
        result
    };
    if let Some((target, _)) = coverage::follow(document, node)? {
        let carry = annotated && node.get("pattern").is_none();
        for variant in descend(("$ref", 0), target, instance, path, carry)? {
            let mut local = node.clone();
            local.as_object_mut().unwrap().remove("$ref");
            conjunct(&mut local, variant);
            out.push(local);
        }
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        for (i, branch) in node[key].as_array().into_iter().flatten().enumerate() {
            if key == "allOf" || accepts(document, branch, instance)? {
                for variant in descend((key, i), branch, instance, path, false)? {
                    let mut local = node.clone();
                    local[key][i] = variant;
                    out.push(local);
                }
            }
        }
    }
    if let Some(condition) = node.get("if") {
        let arm = if accepts(document, condition, instance)? {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = node.get(arm) {
            for variant in descend((arm, 0), branch, instance, path, false)? {
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
                for variant in descend(("properties", 0), child, &instance[name], tail, false)? {
                    let mut local = node.clone();
                    local["properties"][name] = variant;
                    out.push(local);
                }
            }
            for (i, (pattern, child)) in node["patternProperties"]
                .as_object()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let matcher = super::super::validation::pattern_validator(pattern)?;
                if !matcher.is_valid(&json!(name)) {
                    continue;
                }
                selected = true;
                for variant in descend(
                    ("patternProperties", i),
                    child,
                    &instance[name],
                    tail,
                    false,
                )? {
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
                for variant in descend(
                    ("additionalProperties", 0),
                    child,
                    &instance[name],
                    tail,
                    false,
                )? {
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
                for variant in descend(
                    ("array element", *index),
                    child,
                    &instance[*index],
                    tail,
                    false,
                )? {
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

fn permits_type(node: &Value, kind: &str) -> bool {
    match &node["type"] {
        Value::String(t) => t == kind,
        Value::Array(types) => types.iter().any(|t| t == kind),
        _ => node.as_bool() != Some(false),
    }
}

/// Does every way to accept a string at this path impose this exact cap?
/// A conjunction needs one such constraint. A union needs one in every arm that
/// can accept the replacement. Unknown shapes and exhausted work require evidence.
fn bounded_string_path(
    document: &Value,
    schema: &Value,
    instance: &Value,
    path: &[Step],
    cap: (&str, u64),
    remaining: &mut usize,
    depth: usize,
) -> Result<bool> {
    if *remaining == 0 || depth >= 48 {
        return Ok(false);
    }
    *remaining -= 1;
    let kind = match path.first() {
        Some(Step::Key(_)) => "object",
        Some(Step::Index(_)) => "array",
        None => "string",
    };
    for node in super::chain(document, schema)? {
        if !permits_type(node, kind) {
            return Ok(true);
        }
        if let Some((step, tail)) = path.split_first() {
            let (child, value) = match step {
                Step::Key(name) => {
                    let child = super::member_schema(&[node], name)?;
                    if child.is_none()
                        && node.get("patternProperties").is_none()
                        && node["additionalProperties"] == false
                    {
                        return Ok(true);
                    }
                    (child, &instance[name])
                }
                Step::Index(i) => {
                    if node["maxItems"].as_u64().is_some_and(|n| *i as u64 >= n) {
                        return Ok(true);
                    }
                    let child = node["prefixItems"].get(i).or_else(|| node.get("items"));
                    (child.map(std::borrow::Cow::Borrowed), &instance[i])
                }
            };
            if let Some(child) = child
                && bounded_string_path(document, &child, value, tail, cap, remaining, depth + 1)?
            {
                return Ok(true);
            }
        } else if node[cap.0].as_u64() == Some(cap.1) {
            return Ok(true);
        }
        let per_node = *remaining;
        let mut bounded =
            |s: &Value| bounded_string_path(document, s, instance, path, cap, remaining, depth + 1);
        for arm in node["allOf"]
            .as_array()
            .into_iter()
            .flatten()
            .take(per_node)
        {
            if bounded(arm)? {
                return Ok(true);
            }
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(arms) = node[key].as_array()
                && arms.len() <= per_node
            {
                let mut all = true;
                for arm in arms {
                    if !bounded(arm)? {
                        all = false;
                        break;
                    }
                }
                if all {
                    return Ok(true);
                }
            }
        }
        // Changing one existing member preserves required keys and every sibling.
        if let Some(Step::Key(name)) = path.first()
            && let Some(condition) = node.get("if")
            && let Some(fields) = condition.as_object()
            && fields
                .keys()
                .all(|k| matches!(k.as_str(), "properties" | "required"))
            && condition["properties"].get(name).is_none()
        {
            let arm = if accepts(document, condition, instance)? {
                "then"
            } else {
                "else"
            };
            if let Some(s) = node.get(arm)
                && bounded(s)?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Facts that hold for every choice at this instance path. The nodes chosen
/// by a fixture seed are useful for generating probes, but only a conjunction
/// can justify dropping one.
#[derive(Default)]
pub(super) struct ByteDominance {
    pub(super) needs_pattern_witness: bool,
    pub(super) cap: Option<u64>,
    pub(super) max_chars: Option<u64>,
    patterns: std::collections::BTreeSet<String>,
    pub(super) parent_choice: bool,
}

impl ByteDominance {
    pub(super) fn at(
        document: &Value,
        root: &Value,
        instance: &Value,
        path: &[Step],
        nodes: &[&Value],
        supplied: bool,
    ) -> Result<Self> {
        let mut rules = Self::default();
        if nodes.iter().any(|node| {
            node.get(super::MAX_BYTES).is_some() || (supplied && node.get("maxLength").is_some())
        }) {
            let mut remaining = 4096;
            rules.collect(document, root, path, &mut remaining, 0)?;
        }
        if rules.cap.is_none()
            && rules.max_chars.is_none()
            && nodes
                .iter()
                .filter_map(|node| node["pattern"].as_str())
                .any(|p| super::simple_bounds(p).is_none())
        {
            rules.needs_pattern_witness = true;
            let mut remaining = 4096;
            for key in ["maxLength", super::MAX_BYTES] {
                for limit in super::distinct_u64(nodes, key) {
                    if bounded_string_path(
                        document,
                        root,
                        instance,
                        path,
                        (key, limit),
                        &mut remaining,
                        0,
                    )? {
                        rules.needs_pattern_witness = false;
                        return Ok(rules);
                    }
                }
            }
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
                    let mut possible = arms.iter().filter(|arm| permits_type(arm, container));
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
