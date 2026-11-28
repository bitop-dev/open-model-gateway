//! Guardrail rules and effective policies as management JSON. Custom
//! patterns are shown only to readers of the layer's configuration.
use super::policy::*;
use serde_json::{Value, json};
use uuid::Uuid;

// --- Responses -----------------------------------------------------------------

/// A rule as JSON. `patterns` are shown to callers who may read the layer's
/// configuration, else only their count.
pub fn rule_json(id: Uuid, entry: &Entry, show_patterns: bool) -> Value {
    match entry {
        Entry::Rule(r) => {
            let mut detector = json!({
                "kind": r.detector.kind.as_str(),
                "egress": r.detector.kind.egress().as_str(),
                "categories": r.detector.categories.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                "pattern_count": r.detector.patterns.len(),
            });
            if show_patterns {
                detector["patterns"] = json!(r.detector.patterns);
            }
            // The kinds a personal-data rule looks for: configuration every
            // reader of the rule may see (labels, never patterns or text).
            if !r.detector.kinds.is_empty() {
                detector["kinds"] = json!(
                    r.detector
                        .kinds
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                );
            }
            json!({
                "id": id,
                "name": r.name,
                "protection": r.protection.as_str(),
                "detector": detector,
                "input": {"action": r.input_action.as_str(), "redact_scope": r.redact_scope.as_str()},
                "output": {"mode": r.output_mode.as_str()},
                "threshold": r.threshold.map(|t| f64::from(t) / 1000.0),
                "on_error": r.on_error.as_str(),
                "timeout_ms": r.timeout_ms,
                "workloads": r.workloads.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
                "uninspectable": r.uninspectable.as_str(),
            })
        }
        Entry::Tightens(target, t) => {
            let mut v = json!({"id": id, "tightens": target});
            if let Some(a) = t.input_action {
                v["input"] = json!({"action": a.as_str()});
            }
            if let Some(m) = t.output_mode {
                v["output"] = json!({"mode": m.as_str()});
            }
            if let Some(o) = t.on_error {
                v["on_error"] = json!(o.as_str());
            }
            if let Some(th) = t.threshold {
                v["threshold"] = json!(f64::from(th) / 1000.0);
            }
            v
        }
    }
}

/// The effective policy as JSON, with each entry's layer and revision.
pub fn effective_json(effective: &[EffectiveRule], show_patterns: impl Fn(Layer) -> bool) -> Value {
    Value::Array(
        effective
            .iter()
            .map(|e| {
                let mut v = match (&e.rule, e.tightens) {
                    (Some(rule), _) => rule_json(
                        e.id,
                        &Entry::Rule(Box::new(rule.clone())),
                        show_patterns(e.layer),
                    ),
                    (None, Some(target)) => json!({"id": e.id, "tightens": target}),
                    (None, None) => json!({"id": e.id}),
                };
                v["source"] = json!(e.layer.as_str());
                v["rule_set_id"] = json!(e.rule_set_id);
                v["revision"] = json!(e.revision);
                v["tightened_by"] = json!(e.tightened_by);
                v["active"] = json!(e.active());
                v["inactive_reason"] = json!(e.inactive_reason.map(InactiveReason::as_str));
                v
            })
            .collect(),
    )
}

/// What the gateway can run, for `GET /platform/guardrails/detectors`.
pub fn detectors_json() -> Value {
    let names = |v: &[&'static str]| json!(v);
    Value::Array(
        DetectorKind::ALL
            .iter()
            .map(|k| {
                let ready = available::DETECTORS.contains(k);
                json!({
                    "kind": k.as_str(),
                    "egress": k.egress().as_str(),
                    "available": ready,
                    "categories": if ready { names(&available::CATEGORIES.iter().map(|c| c.as_str()).collect::<Vec<_>>()) } else { json!([]) },
                    "input_actions": if ready { names(&available::INPUT_ACTIONS.iter().map(|a| a.as_str()).collect::<Vec<_>>()) } else { json!([]) },
                    "output_modes": if ready { names(&available::OUTPUT_MODES.iter().map(|m| m.as_str()).collect::<Vec<_>>()) } else { json!([]) },
                    "workloads": if ready { names(&available::WORKLOADS.iter().map(|w| w.as_str()).collect::<Vec<_>>()) } else { json!([]) },
                })
            })
            .collect(),
    )
}
