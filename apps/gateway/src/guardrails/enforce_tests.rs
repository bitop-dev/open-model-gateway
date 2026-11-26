//! Decisions of compiled policies on assembled inputs (no database).
use super::*;
use crate::guardrails::policy::{
    Detector, Entry, OutputMode, Protection, RedactScope, Rule, RuleSet, StoredRule, compose,
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
    p.decide(Workload::Generation, Some(input))
        .refusal
        .is_some()
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
    assert!(read.contains(&"opaque-signature"), "a client may send a signature: it is read");
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
