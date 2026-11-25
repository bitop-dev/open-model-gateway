//! Write validation and tighten-only composition of guardrail layers.
use super::*;

fn compile(p: &str) -> bool {
    crate::guardrails::patterns::valid(p)
}
fn body(local_only: Option<bool>, rules: Value) -> LayerIn {
    let mut v = json!({"rules": rules});
    if let Some(l) = local_only {
        v["local_only"] = json!(l);
    }
    serde_json::from_value(v).unwrap()
}
fn secrets(action: &str) -> Value {
    json!({"name":"Secrets","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":action}})
}
fn write(
    layer: Layer,
    platform: Option<&RuleSet>,
    current: Option<&RuleSet>,
    b: LayerIn,
) -> Result<(Option<bool>, Vec<StoredRule>), Refusal> {
    let foreign = HashSet::new();
    validate(
        b,
        &WriteContext {
            layer,
            platform,
            current,
            foreign_ids: &foreign,
            compile: &compile,
        },
    )
}
fn set(layer: Layer, local_only: Option<bool>, rules: Vec<StoredRule>) -> RuleSet {
    RuleSet {
        id: Uuid::now_v7(),
        layer,
        revision: 1,
        local_only,
        rules,
    }
}
fn platform_with(action: &str, local_only: bool) -> RuleSet {
    let (_, rules) = write(
        Layer::TypeDefault,
        None,
        None,
        body(Some(local_only), json!([secrets(action)])),
    )
    .unwrap();
    set(Layer::TypeDefault, Some(local_only), rules)
}
fn only_rule(s: &RuleSet) -> (Uuid, &Rule) {
    match &s.rules[0].entry {
        Entry::Rule(r) => (s.rules[0].id, r),
        Entry::Tightens(..) => panic!("a rule"),
    }
}

#[test]
fn a_secrets_rule_gets_the_designs_defaults() {
    let p = platform_with("block", false);
    let (_, r) = only_rule(&p);
    assert_eq!(r.input_action, InputAction::Block);
    assert_eq!(r.output_mode, OutputMode::Off);
    assert_eq!(r.on_error, OnError::FailClosed, "block rules fail closed");
    assert_eq!(r.redact_scope, RedactScope::UserText);
    assert_eq!(
        r.workloads,
        BTreeSet::from([Workload::Generation, Workload::Embeddings])
    );
    assert_eq!(r.uninspectable, Uninspectable::Deny);
    let m = platform_with("monitor", false);
    assert_eq!(
        only_rule(&m).1.on_error,
        OnError::FailOpen,
        "monitor rules fail open"
    );
    assert_eq!(p.local_only, Some(false));
}

#[test]
fn rules_that_need_a_later_phase_are_refused_not_stored() {
    let variants = [
        json!({"detector":{"kind":"systemone","categories":["moderation"]},"protection":"moderation"}),
        json!({"detector":{"kind":"llm_judge","categories":["moderation"]},"protection":"moderation"}),
        json!({"detector":{"kind":"external","categories":["secret"]}}),
        json!({"detector":{"kind":"pattern","categories":["personal_data"]},"protection":"personal_data"}),
        json!({"input":{"action":"redact"}}),
        json!({"output":{"mode":"monitor"}}),
        json!({"output":{"mode":"block"}}),
        json!({"workloads":["images"]}),
        json!({"workloads":["generation","rerank"]}),
        json!({"uninspectable":"allow"}),
    ];
    for v in variants {
        let mut rule = secrets("block");
        for (k, val) in v.as_object().unwrap() {
            if k == "input" {
                rule["input"] = val.clone();
            } else {
                rule[k] = val.clone();
            }
        }
        for layer in [Layer::TypeDefault, Layer::WorkspaceLocal] {
            let lo = (layer != Layer::WorkspaceLocal).then_some(false);
            assert_eq!(
                write(layer, None, None, body(lo, json!([rule.clone()]))).unwrap_err(),
                Refusal::DetectorUnavailable,
                "{v} on {layer:?}"
            );
        }
    }
}

#[test]
fn local_only_refuses_egress_on_write_before_availability() {
    let rule = json!({"name":"Mod","protection":"moderation","detector":{"kind":"systemone","categories":["moderation"]},"input":{"action":"block"}});
    // On the platform layer that sets it.
    assert_eq!(
        write(
            Layer::WorkspaceOverride,
            None,
            None,
            body(Some(true), json!([rule.clone()]))
        )
        .unwrap_err(),
        Refusal::EgressNotAllowed
    );
    assert_eq!(
        write(
            Layer::WorkspaceOverride,
            None,
            None,
            body(Some(false), json!([rule.clone()]))
        )
        .unwrap_err(),
        Refusal::DetectorUnavailable
    );
    // On a local layer under it.
    let closed = platform_with("monitor", true);
    let open = platform_with("monitor", false);
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&closed),
            None,
            body(None, json!([rule.clone()]))
        )
        .unwrap_err(),
        Refusal::EgressNotAllowed
    );
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&open),
            None,
            body(None, json!([rule]))
        )
        .unwrap_err(),
        Refusal::DetectorUnavailable
    );
    // The pattern detector sends nothing out.
    assert!(
        write(
            Layer::WorkspaceLocal,
            Some(&closed),
            None,
            body(None, json!([secrets("block")]))
        )
        .is_ok()
    );
    // `local_only` belongs to platform layers.
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&open),
            None,
            body(Some(true), json!([]))
        )
        .unwrap_err(),
        Refusal::Invalid
    );
}

#[test]
fn a_local_layer_tightens_but_never_loosens_a_platform_rule() {
    let p = platform_with("monitor", false);
    let (target, _) = only_rule(&p);
    let tighten = |v: Value| {
        let mut e = json!({"tightens": target});
        for (k, val) in v.as_object().unwrap() {
            e[k] = val.clone();
        }
        write(
            Layer::WorkspaceLocal,
            Some(&p),
            None,
            body(None, json!([e])),
        )
    };
    assert!(tighten(json!({"input":{"action":"block"}})).is_ok());
    assert!(
        tighten(json!({"input":{"action":"monitor"},"on_error":"fail_closed"})).is_ok(),
        "same action, stricter failure"
    );
    assert_eq!(
        tighten(json!({"input":{"action":"off"}})).unwrap_err(),
        Refusal::LoosenNotAllowed(target)
    );
    assert_eq!(
        tighten(json!({"input":{"action":"redact"}})).unwrap_err(),
        Refusal::DetectorUnavailable
    );
    assert_eq!(
        tighten(json!({"threshold":0.5})).unwrap_err(),
        Refusal::Invalid,
        "pattern rules have no threshold"
    );
    assert_eq!(
        tighten(json!({"name":"x","input":{"action":"block"}})).unwrap_err(),
        Refusal::Invalid,
        "only what it tightens"
    );
    assert_eq!(
        tighten(json!({})).unwrap_err(),
        Refusal::Invalid,
        "something to tighten"
    );
    let block = platform_with("block", false);
    let (strict, _) = only_rule(&block);
    let e = json!({"tightens": strict, "input": {"action": "monitor"}});
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&block),
            None,
            body(None, json!([e]))
        )
        .unwrap_err(),
        Refusal::LoosenNotAllowed(strict)
    );
    let e = json!({"tightens": strict, "on_error": "fail_open"});
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&block),
            None,
            body(None, json!([e]))
        )
        .unwrap_err(),
        Refusal::LoosenNotAllowed(strict)
    );
    // A platform rule is never redefined or tightened from a platform layer.
    let mut copy = secrets("off");
    copy["id"] = json!(target);
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&p),
            None,
            body(None, json!([copy]))
        )
        .unwrap_err(),
        Refusal::LoosenNotAllowed(target)
    );
    let e = json!({"tightens": target, "input": {"action": "block"}});
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([e]))
        )
        .unwrap_err(),
        Refusal::Invalid
    );
    // An unknown platform rule, and two entries for one rule.
    let e = json!({"tightens": Uuid::now_v7(), "input": {"action": "block"}});
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&p),
            None,
            body(None, json!([e]))
        )
        .unwrap_err(),
        Refusal::Invalid
    );
    let e = json!({"tightens": target, "input": {"action": "block"}});
    assert_eq!(
        write(
            Layer::WorkspaceLocal,
            Some(&p),
            None,
            body(None, json!([e.clone(), e]))
        )
        .unwrap_err(),
        Refusal::Invalid
    );
}

#[test]
fn composition_takes_the_stricter_value_so_a_stale_entry_never_loosens() {
    let p = platform_with("monitor", false);
    let (target, _) = only_rule(&p);
    let (_, local) = write(
        Layer::WorkspaceLocal,
        Some(&p),
        None,
        body(
            None,
            json!([{"tightens": target, "input": {"action": "block"}}, secrets("monitor")]),
        ),
    )
    .unwrap();
    let local = set(Layer::WorkspaceLocal, None, local);
    let eff = compose(Some(&p), Some(&local));
    assert_eq!(
        eff.len(),
        2,
        "the platform rule (tightened) and the local rule"
    );
    assert_eq!(eff[0].id, target);
    assert_eq!(
        eff[0].rule.as_ref().unwrap().input_action,
        InputAction::Block
    );
    assert_eq!(eff[0].tightened_by, vec![local.rules[0].id]);
    assert_eq!(eff[1].layer, Layer::WorkspaceLocal);
    // The platform rule later became stricter than the entry: the entry, now
    // weaker, changes nothing.
    let mut stricter = p.clone();
    if let Entry::Rule(r) = &mut stricter.rules[0].entry {
        r.input_action = InputAction::Block;
        r.on_error = OnError::FailClosed;
    }
    let stale = set(
        Layer::WorkspaceLocal,
        None,
        vec![StoredRule {
            id: Uuid::now_v7(),
            entry: Entry::Tightens(
                target,
                Tightening {
                    input_action: Some(InputAction::Monitor),
                    on_error: Some(OnError::FailOpen),
                    ..Default::default()
                },
            ),
        }],
    );
    let eff = compose(Some(&stricter), Some(&stale));
    let r = eff[0].rule.as_ref().unwrap();
    assert_eq!(
        (r.input_action, r.on_error),
        (InputAction::Block, OnError::FailClosed)
    );
    // The platform rule was removed: the entry stays, inactive.
    let empty = set(Layer::TypeDefault, Some(false), Vec::new());
    let eff = compose(Some(&empty), Some(&stale));
    assert_eq!(eff.len(), 1);
    assert!(!eff[0].active());
    assert_eq!(
        eff[0].inactive_reason,
        Some(InactiveReason::TightenedRuleMissing)
    );
    assert_eq!(eff[0].tightens, Some(target));
}

#[test]
fn local_only_makes_an_egress_rule_inactive_when_composing() {
    // A local rule written (by an earlier release, or before the platform set
    // `local_only`) with a detector that sends text out.
    let mut rule = only_rule(&platform_with("block", false)).1.clone();
    rule.detector.kind = DetectorKind::Systemone;
    rule.detector.categories = BTreeSet::from([Category::Moderation]);
    let local = set(
        Layer::WorkspaceLocal,
        None,
        vec![
            StoredRule {
                id: Uuid::now_v7(),
                entry: Entry::Rule(Box::new(rule)),
            },
            platform_with("block", false).rules[0].clone(),
        ],
    );
    let open = compose(Some(&platform_with("monitor", false)), Some(&local));
    assert!(open.iter().all(EffectiveRule::active));
    let closed = compose(Some(&platform_with("monitor", true)), Some(&local));
    assert_eq!(closed.len(), 3, "never dropped");
    assert_eq!(
        closed[1].inactive_reason,
        Some(InactiveReason::EgressNotAllowed)
    );
    assert!(
        closed[0].active() && closed[2].active(),
        "local rules keep running"
    );
}

#[test]
fn custom_patterns_are_counted_per_layer_and_compiled() {
    let custom = |patterns: Vec<String>| json!({"name":"DLP","protection":"custom","detector":{"kind":"pattern","categories":["custom"],"patterns":patterns},"input":{"action":"block"}});
    let ok = write(
        Layer::TypeDefault,
        None,
        None,
        body(Some(false), json!([custom(vec![r"PROJECT-\d{4}".into()])])),
    );
    assert!(ok.is_ok());
    let thirty: Vec<String> = (0..30).map(|i| format!("code{i}")).collect();
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([custom(thirty.clone()), custom(thirty)]))
        )
        .unwrap_err(),
        Refusal::Invalid,
        "50 per layer"
    );
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([custom(vec!["(unclosed".into()])]))
        )
        .unwrap_err(),
        Refusal::PatternInvalid
    );
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([custom(vec!["a".repeat(513)])]))
        )
        .unwrap_err(),
        Refusal::Invalid,
        "512 bytes each"
    );
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([custom(vec![])]))
        )
        .unwrap_err(),
        Refusal::Invalid,
        "a custom rule has patterns"
    );
    let mut built_in = secrets("block");
    built_in["detector"]["patterns"] = json!(["x"]);
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([built_in]))
        )
        .unwrap_err(),
        Refusal::Invalid,
        "built-in categories carry none"
    );
    // Shown to readers of the layer's configuration, else counted.
    let (_, rules) = ok.unwrap();
    let hidden = rule_json(rules[0].id, &rules[0].entry, false);
    assert_eq!(hidden["detector"]["pattern_count"], 1);
    assert!(hidden["detector"].get("patterns").is_none());
    assert!(!hidden.to_string().contains("PROJECT"));
    assert_eq!(
        rule_json(rules[0].id, &rules[0].entry, true)["detector"]["patterns"],
        json!([r"PROJECT-\d{4}"])
    );
}

#[test]
fn ids_are_kept_assigned_or_refused() {
    let p = platform_with("block", false);
    let (id, _) = only_rule(&p);
    let mut same = secrets("monitor");
    same["id"] = json!(id);
    let (_, rules) = write(
        Layer::TypeDefault,
        None,
        Some(&p),
        body(Some(false), json!([same.clone(), secrets("block")])),
    )
    .unwrap();
    assert_eq!(rules[0].id, id, "an existing rule keeps its id");
    assert_ne!(rules[1].id, id);
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            Some(&p),
            body(Some(false), json!([same.clone(), same]))
        )
        .unwrap_err(),
        Refusal::Invalid,
        "one entry per id"
    );
    let mut invented = secrets("block");
    invented["id"] = json!(Uuid::now_v7());
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            Some(&p),
            body(Some(false), json!([invented]))
        )
        .unwrap_err(),
        Refusal::Invalid
    );
    let too_many: Vec<Value> = (0..=MAX_RULES).map(|_| secrets("block")).collect();
    assert_eq!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!(too_many))
        )
        .unwrap_err(),
        Refusal::Invalid
    );
}

#[test]
fn malformed_rules_are_plain_invalid() {
    for bad in [
        json!({"protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"}}),
        json!({"name":"","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"}}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":[]},"input":{"action":"block"}}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret","secret"]},"input":{"action":"block"}}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]}}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"},"timeout_ms":100}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"},"extra":1}),
        json!({"name":"x","protection":"nope","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"}}),
    ] {
        assert_eq!(
            write(
                Layer::TypeDefault,
                None,
                None,
                body(Some(false), json!([bad.clone()]))
            )
            .unwrap_err(),
            Refusal::Invalid,
            "{bad}"
        );
    }
}

#[test]
fn null_and_positional_forms_are_refused() {
    // A positional array is never read into an object, at any level.
    assert!(serde_json::from_value::<LayerIn>(json!([false, []])).is_err());
    for bad in [
        json!({"name":"x","protection":"secrets","detector":["pattern",["secret"]],"input":{"action":"block"}}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":["block"]}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"},"output":[]}),
        json!(["x", "secrets"]),
        // Null only where the schema allows it (threshold, timeout_ms).
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block","redact_scope":null}}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"},"output":null}),
        json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"},"workloads":null}),
    ] {
        assert_eq!(
            write(
                Layer::TypeDefault,
                None,
                None,
                body(Some(false), json!([bad.clone()]))
            )
            .unwrap_err(),
            Refusal::Invalid,
            "{bad}"
        );
    }
    let ok = json!({"name":"x","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":"block"},"threshold":null,"timeout_ms":null});
    assert!(
        write(
            Layer::TypeDefault,
            None,
            None,
            body(Some(false), json!([ok]))
        )
        .is_ok()
    );
    assert!(
        serde_json::from_value::<LayerIn>(json!({"local_only":null,"rules":[]}))
            .map(|b| validate(
                b,
                &WriteContext {
                    layer: Layer::TypeDefault,
                    platform: None,
                    current: None,
                    foreign_ids: &HashSet::new(),
                    compile: &compile
                }
            ))
            .unwrap()
            .is_err()
    );
}
