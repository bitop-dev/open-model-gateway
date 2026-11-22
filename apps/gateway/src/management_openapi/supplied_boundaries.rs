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
    edge: (&str, &Value),
    depth: usize,
) -> Result<Vec<Value>> {
    ensure!(depth < 48, "supplied boundary schema nesting too deep");
    let mut out = Vec::new();
    let (keyword, bound) = edge;
    // The counterfactual evaluates a single refused value here. Omitting the
    // keyword gives exactly its verdict with an exception for that value.
    if path.is_empty() && node.get(keyword) == Some(bound) {
        let mut relaxed = node.clone();
        relaxed.as_object_mut().unwrap().remove(keyword);
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
