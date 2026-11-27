//! Decisions of compiled policies on assembled inputs (no database).
use super::*;
use crate::guardrails::policy::{
    Detector, Entry, OutputMode, Protection, RedactScope, Rule, RuleSet, StoredRule, Tightening,
    compose,
};
use crate::inference::types::{
    ChatRequest, FunctionTool, Message, MessagesOptions, Role, ToolCall,
};
use serde_json::json;

fn rule(categories: &[Category], patterns: &[&str], action: InputAction) -> Rule {
    Rule {
        name: "R".into(),
        protection: if patterns.is_empty() {
            Protection::Secrets
        } else {
            Protection::Custom
        },
        detector: Detector {
            kind: DetectorKind::Pattern,
            categories: categories.iter().copied().collect(),
            patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
            reference: None,
        },
        input_action: action,
        redact_scope: RedactScope::UserText,
        output_mode: OutputMode::Off,
        threshold: None,
        on_error: if action == InputAction::Block {
            OnError::FailClosed
        } else {
            OnError::FailOpen
        },
        timeout_ms: None,
        workloads: [Workload::Generation, Workload::Embeddings]
            .into_iter()
            .collect(),
        uninspectable: Uninspectable::Deny,
    }
}
fn policy(rules: Vec<Rule>) -> Policy {
    let set = RuleSet {
        id: Uuid::now_v7(),
        layer: Layer::TypeDefault,
        revision: 3,
        local_only: Some(false),
        rules: rules
            .into_iter()
            .map(|r| StoredRule {
                id: Uuid::now_v7(),
                entry: Entry::Rule(Box::new(r)),
            })
            .collect(),
    };
    Policy::compile(&compose(Some(&set), None))
}
fn key() -> String {
    let hex: String = "0123456789abcdef".chars().cycle().take(96).collect();
    format!("{}_{}.{}", "omg", &hex[..32], &hex[32..])
}
fn text(t: &str) -> Inspection<'_> {
    let mut input = Inspection::default();
    input.push(t, Part::Content);
    input
}

#[test]
fn a_block_rule_refuses_with_ids_and_a_count() {
    let p = policy(vec![rule(&[Category::Secret], &[], InputAction::Block)]);
    let k = key();
    let body = format!("{k} and again {k}");
    let d = p.decide(Workload::Generation, Some(&text(&body)));
    let refusal = d.refusal.expect("refused");
    assert_eq!(refusal.kind, RefusalKind::InputBlocked);
    assert_eq!(refusal.category, Category::Secret);
    assert_eq!(d.events.len(), 1);
    assert_eq!(
        (d.events[0].action, d.events[0].count, d.events[0].revision),
        (Action::Blocked, 1, 3)
    );
    assert!(d.events[0].refused, "the refusing decision is marked");
}

#[test]
fn monitor_records_and_lets_the_request_through() {
    let p = policy(vec![rule(
        &[Category::Custom],
        &["PROJECT-[0-9]{4}"],
        InputAction::Monitor,
    )]);
    let d = p.decide(
        Workload::Embeddings,
        Some(&text("PROJECT-1234, PROJECT-5678")),
    );
    assert!(d.refusal.is_none());
    assert_eq!(
        (d.events[0].action, d.events[0].count),
        (Action::Monitored, 2)
    );
    assert!(!d.events[0].refused);
    assert!(
        p.decide(Workload::Generation, Some(&text("nothing here")))
            .events
            .is_empty()
    );
}

#[test]
fn off_rules_and_other_workloads_do_not_run() {
    let p = policy(vec![rule(&[Category::Secret], &[], InputAction::Off)]);
    assert!(p.is_empty());
    let mut only_generation = rule(&[Category::Secret], &[], InputAction::Block);
    only_generation.workloads = [Workload::Generation].into_iter().collect();
    let p = policy(vec![only_generation]);
    assert!(!p.applies_to(Workload::Embeddings));
    let k = key();
    assert!(
        p.decide(Workload::Embeddings, Some(&text(&k)))
            .refusal
            .is_none()
    );
}

#[test]
fn unreadable_content_is_refused_by_an_enforcing_rule_and_skipped_by_a_monitor() {
    let block = policy(vec![rule(&[Category::Secret], &[], InputAction::Block)]);
    let d = block.decide(Workload::Generation, None);
    assert_eq!(d.refusal.map(|r| r.kind), Some(RefusalKind::NotSupported));
    let monitor = policy(vec![rule(&[Category::Secret], &[], InputAction::Monitor)]);
    let d = monitor.decide(Workload::Generation, None);
    assert!(d.refusal.is_none());
    assert_eq!(d.events[0].action, Action::Skipped);
}

#[test]
fn a_finding_outranks_an_unreadable_part() {
    let p = policy(vec![rule(&[Category::Secret], &[], InputAction::Block)]);
    let k = key();
    let mut input = text(&k);
    input.uninspectable = true;
    let d = p.decide(Workload::Generation, Some(&input));
    assert_eq!(d.refusal.map(|r| r.kind), Some(RefusalKind::InputBlocked));
}

#[test]
fn a_rule_this_release_cannot_run_fails_under_its_on_error() {
    let mut later = rule(&[Category::Moderation], &[], InputAction::Block);
    later.protection = Protection::Moderation;
    later.detector.kind = DetectorKind::Systemone;
    let p = policy(vec![later.clone()]);
    let d = p.decide(Workload::Generation, Some(&text("hello")));
    assert_eq!(d.refusal.map(|r| r.kind), Some(RefusalKind::DetectorFailed));
    assert_eq!(d.events[0].action, Action::DetectorFailed);
    // Also when it enforces only on output (input off).
    let mut output_only = rule(&[Category::Secret], &[], InputAction::Off);
    output_only.output_mode = OutputMode::Block;
    output_only.on_error = OnError::FailClosed;
    let d = policy(vec![output_only]).decide(Workload::Generation, Some(&text("hello")));
    assert_eq!(d.refusal.map(|r| r.kind), Some(RefusalKind::DetectorFailed));
    later.on_error = OnError::FailOpen;
    later.input_action = InputAction::Monitor;
    let d = policy(vec![later]).decide(Workload::Generation, Some(&text("hello")));
    assert!(d.refusal.is_none());
    assert_eq!(d.events[0].action, Action::DetectorFailed);
}

fn texts<'a>(input: &'a Inspection<'_>) -> Vec<&'a str> {
    input.segments.iter().map(|s| s.text.as_ref()).collect()
}
fn found(input: &Inspection<'_>) -> bool {
    let p = policy(vec![rule(&[Category::Secret], &[], InputAction::Block)]);
    let rule_id = p.rules[0].id;
    match p.decide(Workload::Generation, Some(input)).refusal {
        Some(r) => {
            assert_eq!(
                (r.kind, r.rule_id, r.category, r.detector),
                (
                    RefusalKind::InputBlocked,
                    rule_id,
                    Category::Secret,
                    DetectorKind::Pattern
                )
            );
            true
        }
        None => false,
    }
}
fn request(
    messages: Vec<Message>,
    tools: Vec<FunctionTool>,
    options: MessagesOptions,
) -> ChatRequest {
    ChatRequest {
        messages_options: Some(Box::new(options)),
        model: "m".into(),
        messages,
        tools,
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        stream: false,
    }
}
fn said(role: Role, content: &str) -> Message {
    Message {
        role,
        content: Some(content.into()),
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_result_is_error: false,
    }
}

#[test]
fn native_envelopes_are_read_and_images_are_not() {
    let k = key();
    let native = json!({"role":"user","content":[
        {"type":"text","text": format!("see {k}")},
        {"type":"document","source":{"type":"text","media_type":"text/plain","data":"plain notes"}},
        {"type":"thinking","thinking":"t","signature":"opaque-signature"}
    ]});
    let mut input = Inspection::default();
    message(&native, &mut input);
    let read = texts(&input);
    assert!(read.iter().any(|t| t.contains(&k)));
    assert!(read.contains(&"plain notes"));
    assert!(
        read.contains(&"opaque-signature"),
        "a client may send a signature: it is read"
    );
    assert!(!input.uninspectable);
    let image = json!({"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo"}}]});
    let mut input = Inspection::default();
    message(&image, &mut input);
    assert!(input.uninspectable);
    assert!(input.segments.is_empty(), "image data is never scanned");
}

#[test]
fn text_in_any_json_key_of_a_tool_or_tool_input_is_read() {
    let k = key();
    // A schema property named like a block key, and a tool input with one.
    let schema = json!({"type":"object","properties":{"type":{"type":"string","description":k},"role":{"enum":[k]}}});
    let named = json!({"type":"object","properties":{k.clone():{"type":"string"}}});
    let tool = FunctionTool {
        name: "t".into(),
        description: None,
        parameters: named,
        strict: None,
    };
    assert!(
        found(&chat_inspection(&request(
            Vec::new(),
            vec![tool],
            MessagesOptions::default()
        ))),
        "a property name"
    );
    let tool = FunctionTool {
        name: "t".into(),
        description: None,
        parameters: schema,
        strict: None,
    };
    assert!(found(&chat_inspection(&request(
        Vec::new(),
        vec![tool],
        MessagesOptions::default()
    ))));
    let native = MessagesOptions {
        native_messages: vec![
            json!({"role":"assistant","content":[{"type":"tool_use","id":"x","name":"t","input":{"signature":k,"source":{"type":"text","note":k}}}]}),
        ],
        ..MessagesOptions::default()
    };
    let sent = request(Vec::new(), Vec::new(), native);
    let input = chat_inspection(&sent);
    assert!(found(&input));
    assert!(!input.uninspectable, "a tool input is not an attachment");
}

#[test]
fn escaped_arguments_ids_stop_sequences_and_split_blocks_are_read() {
    let k = key();
    let escaped = format!("{{\"k\":\"\\u006f{}\"}}", &k[1..]);
    let mut assistant = said(Role::Assistant, "");
    assistant.tool_calls.push(ToolCall {
        id: "call".into(),
        name: "t".into(),
        arguments: escaped,
    });
    assert!(
        found(&chat_inspection(&request(
            vec![assistant],
            Vec::new(),
            MessagesOptions::default()
        ))),
        "decoded arguments"
    );
    let mut result = said(Role::Tool, "ok");
    result.tool_call_id = Some(k.clone());
    assert!(
        found(&chat_inspection(&request(
            vec![result],
            Vec::new(),
            MessagesOptions::default()
        ))),
        "tool call id"
    );
    let stops = MessagesOptions {
        stop_sequences: Some(vec![k.clone()]),
        ..MessagesOptions::default()
    };
    assert!(
        found(&chat_inspection(&request(Vec::new(), Vec::new(), stops))),
        "stop sequence"
    );
    let (head, tail) = k.split_at(20);
    let split = vec![
        said(Role::System, head),
        said(Role::System, tail),
        said(Role::User, "hi"),
    ];
    assert!(
        found(&chat_inspection(&request(
            split,
            Vec::new(),
            MessagesOptions::default()
        ))),
        "split system"
    );
    let blocks = MessagesOptions {
        system: Some(json!([{"type":"text","text":head},{"type":"text","text":tail}])),
        ..MessagesOptions::default()
    };
    assert!(
        found(&chat_inspection(&request(Vec::new(), Vec::new(), blocks))),
        "split blocks"
    );
}

fn set(layer: Layer, local_only: Option<bool>, entries: Vec<(Uuid, Entry)>) -> RuleSet {
    RuleSet {
        id: Uuid::now_v7(),
        layer,
        revision: 7,
        local_only,
        rules: entries
            .into_iter()
            .map(|(id, entry)| StoredRule { id, entry })
            .collect(),
    }
}
fn ruled(r: Rule) -> (Uuid, Entry) {
    (Uuid::now_v7(), Entry::Rule(Box::new(r)))
}

#[test]
fn composed_layers_are_enforced_with_tightenings_and_local_only() {
    let monitor_id = Uuid::now_v7();
    let mut later = rule(&[Category::Moderation], &[], InputAction::Block);
    later.protection = Protection::Moderation;
    later.detector.kind = DetectorKind::Systemone;
    let platform = |local_only| {
        set(
            Layer::WorkspaceOverride,
            Some(local_only),
            vec![
                (
                    monitor_id,
                    Entry::Rule(Box::new(rule(
                        &[Category::Secret],
                        &[],
                        InputAction::Monitor,
                    ))),
                ),
                ruled(later.clone()),
            ],
        )
    };
    let tighten_id = Uuid::now_v7();
    let local = set(
        Layer::WorkspaceLocal,
        None,
        vec![
            (
                tighten_id,
                Entry::Tightens(
                    monitor_id,
                    Tightening {
                        input_action: Some(InputAction::Block),
                        output_mode: None,
                        on_error: None,
                        threshold: None,
                    },
                ),
            ),
            ruled(rule(
                &[Category::Custom],
                &["ACME-[0-9]{4}"],
                InputAction::Monitor,
            )),
        ],
    );
    let k = key();
    let body = format!("{k} ACME-1234");
    // local_only: the egress rule does not run; the tightened platform rule
    // blocks and says it was tightened; the local custom rule monitors.
    let p = Policy::compile(&compose(Some(&platform(true)), Some(&local)));
    assert!(p.has_local_custom());
    let d = p.decide(Workload::Generation, Some(&text(&body)));
    let refusal = d.refusal.expect("the tightening blocks");
    assert_eq!(
        (refusal.kind, refusal.rule_id, refusal.category),
        (RefusalKind::InputBlocked, monitor_id, Category::Secret)
    );
    let summary: Vec<_> = d
        .events
        .iter()
        .map(|e| (e.layer, e.action, e.category, e.tightened, e.refused))
        .collect();
    assert_eq!(
        summary,
        [
            (
                Layer::WorkspaceOverride,
                Action::Blocked,
                Category::Secret,
                true,
                true
            ),
            (
                Layer::WorkspaceLocal,
                Action::Monitored,
                Category::Custom,
                false,
                false
            ),
        ]
    );
    // Without local_only the later-release rule runs under its on_error and
    // the block outranks its failure.
    let p = Policy::compile(&compose(Some(&platform(false)), Some(&local)));
    let d = p.decide(Workload::Generation, Some(&text(&body)));
    assert_eq!(d.refusal.map(|r| r.kind), Some(RefusalKind::InputBlocked));
    assert!(
        d.events
            .iter()
            .any(|e| e.action == Action::DetectorFailed && !e.refused)
    );
    let d = p.decide(Workload::Generation, Some(&text("clean")));
    let refusal = d.refusal.expect("fail-closed");
    assert_eq!(refusal.kind, RefusalKind::DetectorFailed);
    assert_eq!(refusal.detector, DetectorKind::Systemone);
}

#[test]
fn the_strongest_refusal_of_several_rules_answers() {
    let mut later = rule(&[Category::Moderation], &[], InputAction::Block);
    later.protection = Protection::Moderation;
    later.detector.kind = DetectorKind::Systemone;
    let custom = rule(&[Category::Custom], &["ACME-[0-9]{4}"], InputAction::Block);
    let secret = rule(&[Category::Secret], &[], InputAction::Block);
    let ids = |p: &Policy| p.rules.iter().map(|r| r.id).collect::<Vec<_>>();
    // Not supported < detector failed < unavailable < blocked, in any order.
    let mut input = text("ACME-0001");
    input.uninspectable = true;
    for order in [
        vec![later.clone(), custom.clone(), secret.clone()],
        vec![secret.clone(), custom.clone(), later.clone()],
    ] {
        let p = policy(order);
        let rules = ids(&p);
        let d = p.decide(Workload::Generation, Some(&input));
        let refusal = d.refusal.unwrap();
        assert_eq!(refusal.kind, RefusalKind::InputBlocked);
        assert_eq!(refusal.category, Category::Custom);
        let custom_id = rules[p.rules.iter().position(|r| r.custom).unwrap()];
        assert_eq!(refusal.rule_id, custom_id);
        assert_eq!(d.events.iter().filter(|e| e.refused).count(), 1);
        // Without a match: the custom rule's spent budget (unavailable)
        // outranks the later rule's failure and the secret rule's unreadable part.
        let d = p.decide_with(Workload::Generation, Some(&input), &Budget::steps(0));
        let refusal = d.refusal.unwrap();
        assert_eq!(
            (refusal.kind, refusal.rule_id),
            (RefusalKind::Unavailable, custom_id)
        );
        let d = p.decide(Workload::Generation, Some(&text("clean")));
        assert_eq!(d.refusal.unwrap().kind, RefusalKind::DetectorFailed);
    }
    let d = policy(vec![secret]).decide(Workload::Generation, Some(&input));
    assert_eq!(d.refusal.unwrap().kind, RefusalKind::NotSupported);
}

#[test]
fn a_spent_budget_is_unavailable_whatever_on_error_and_keeps_its_findings() {
    for on_error in [OnError::FailOpen, OnError::FailClosed] {
        for action in [InputAction::Monitor, InputAction::Block] {
            let mut r = rule(&[Category::Custom], &["ACME-[0-9]{4}|zzzz"], action);
            r.on_error = on_error;
            let p = policy(vec![r]);
            let id = p.rules[0].id;
            let body = format!("ACME-0001 {}", "x".repeat(20_000));
            let d = p.decide_with(Workload::Generation, Some(&text(&body)), &Budget::steps(1));
            let refusal = d.refusal.expect("never forwarded uninspected");
            // A match found before the budget ran out still blocks.
            let kind = if action == InputAction::Block {
                RefusalKind::InputBlocked
            } else {
                RefusalKind::Unavailable
            };
            assert_eq!(
                (refusal.kind, refusal.rule_id),
                (kind, id),
                "{on_error:?} {action:?}"
            );
            let kept = if action == InputAction::Block {
                Action::Blocked
            } else {
                Action::Monitored
            };
            let mut actions: Vec<_> = d.events.iter().map(|e| (e.action, e.count)).collect();
            actions.sort_by_key(|(a, _)| a.as_str());
            let mut expected = vec![(Action::DetectorFailed, 1), (kept, 1)];
            expected.sort_by_key(|(a, _)| a.as_str());
            assert_eq!(actions, expected, "{on_error:?} {action:?}");
        }
    }
}

#[test]
fn no_slot_is_unavailable_for_every_rule_and_a_detector_failure_follows_on_error() {
    let mut open = rule(&[Category::Secret], &[], InputAction::Monitor);
    open.on_error = OnError::FailOpen;
    let p = policy(vec![open.clone()]);
    let d = p.unavailable(Workload::Generation);
    assert_eq!(d.refusal.map(|r| r.kind), Some(RefusalKind::Unavailable));
    assert_eq!(
        d.refusal.map(|r| r.scanned),
        Some(false),
        "nothing was scanned"
    );
    assert!(d.events[0].refused);
    // A spent budget is the input's own: scanned.
    let custom = policy(vec![rule(
        &[Category::Custom],
        &["zzzz"],
        InputAction::Block,
    )]);
    let d = custom.decide_with(Workload::Generation, Some(&text("abc")), &Budget::steps(0));
    assert_eq!(
        d.refusal.map(|r| (r.kind, r.scanned)),
        Some((RefusalKind::Unavailable, true))
    );
    assert!(
        policy(vec![open.clone()])
            .unavailable(Workload::Rerank)
            .refusal
            .is_none()
    );
    // A rule this release cannot run stays a detector failure under its own
    // on_error: fail-open records it and does not refuse; beside a runnable
    // rule, the request is unavailable (and that rule is the one named).
    let mut later = rule(&[Category::Moderation], &[], InputAction::Monitor);
    later.protection = Protection::Moderation;
    later.detector.kind = DetectorKind::Systemone;
    later.on_error = OnError::FailOpen;
    let d = policy(vec![later.clone()]).unavailable(Workload::Generation);
    assert!(d.refusal.is_none());
    assert_eq!(d.events[0].action, Action::DetectorFailed);
    let p = policy(vec![later.clone(), open]);
    let runnable = p.rules[1].id;
    let d = p.unavailable(Workload::Generation);
    let refusal = d.refusal.unwrap();
    assert_eq!(
        (refusal.kind, refusal.rule_id),
        (RefusalKind::Unavailable, runnable)
    );
    later.on_error = OnError::FailClosed;
    let d = policy(vec![later]).unavailable(Workload::Generation);
    assert_eq!(d.refusal.unwrap().kind, RefusalKind::DetectorFailed);
}

#[test]
fn every_option_string_a_messages_request_forwards_is_read() {
    let k = key();
    let value = json!({"note": k});
    for options in [
        MessagesOptions {
            beta_headers: vec![format!("context-1m-2025-08-07,{k}")],
            ..MessagesOptions::default()
        },
        MessagesOptions {
            thinking: Some(value.clone()),
            ..MessagesOptions::default()
        },
        MessagesOptions {
            task_budget: Some(value.clone()),
            ..MessagesOptions::default()
        },
        MessagesOptions {
            cache_controls: vec![value.clone()],
            ..MessagesOptions::default()
        },
        MessagesOptions {
            defer_loading: vec![value.clone()],
            ..MessagesOptions::default()
        },
        MessagesOptions {
            history_thinking: vec![value.clone()],
            ..MessagesOptions::default()
        },
    ] {
        let r = request(vec![said(Role::User, "hi")], Vec::new(), options);
        assert!(found(&chat_inspection(&r)));
    }
}

#[test]
fn text_parts_are_read_alone_and_joined_and_an_image_part_is_uninspectable() {
    use crate::inference::types::{Content, ImageInput, ImageType, Part as ContentPart};
    let k = key();
    let (head, tail) = k.split_at(20);
    let mut m = said(Role::User, "x");
    m.content = Some(Content::Parts(vec![
        ContentPart::Text(head.to_owned()),
        ContentPart::Image(ImageInput::new(
            "data:image/png;base64,iVBORw0KGgo=".into(),
            ImageType::Png,
            None,
            8,
            None,
        )),
        ContentPart::Text(tail.to_owned()),
    ]));
    let r = request(vec![m], Vec::new(), MessagesOptions::default());
    let input = chat_inspection(&r);
    assert!(input.uninspectable, "an image cannot be read");
    assert!(
        texts(&input).contains(&k.as_str()),
        "the split secret is read joined"
    );
    assert!(
        texts(&input).iter().all(|t| !t.contains("iVBOR")),
        "nothing of the image is read"
    );
    assert!(found(&input));
}
