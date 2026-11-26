//! Input enforcement (docs/guardrails-design.md §3 "Input", §6): a
//! workspace's effective rules compiled for the engine, the text a request
//! sends upstream, and the decision for one request.
//!
//! Every rule runs on its own and the strictest outcome wins. A rule this
//! release cannot run (its detector, category, action or workload needs a
//! later release) is never skipped silently: it fails like a detector, under
//! its own `on_error`. The decision holds rule ids, codes and counts, never
//! matched text.
use std::collections::BTreeSet;

use serde_json::Value;
use uuid::Uuid;

use super::detector::{Inspection, Part};
use super::policy::{
    Category, DetectorKind, EffectiveRule, InactiveReason, InputAction, Layer, OnError,
    Uninspectable, Workload,
};
use super::secrets::PatternDetector;
use crate::inference::types::{ChatRequest, Role};

enum Runner {
    Pattern(PatternDetector),
    /// Stored by a later release, or a pattern that no longer compiles.
    CannotRun,
}

/// One effective rule, ready to run.
pub struct CompiledRule {
    pub id: Uuid,
    pub rule_set_id: Uuid,
    pub layer: Layer,
    pub revision: i64,
    pub kind: DetectorKind,
    pub categories: Vec<Category>,
    action: InputAction,
    on_error: OnError,
    workloads: BTreeSet<Workload>,
    uninspectable: Uninspectable,
    runner: Runner,
}

/// A workspace's effective input rules. Empty when nothing applies, which is
/// the common case: the engine then does no work.
#[derive(Default)]
pub struct Policy {
    rules: Vec<CompiledRule>,
}
impl Policy {
    /// The rules that run: active entries, and entries this release cannot
    /// run (which fail under their `on_error`). Inactive entries (egress under
    /// `local_only`, a dangling tightening) and rules whose input action is
    /// `off` do not run.
    pub fn compile(effective: &[EffectiveRule]) -> Self {
        let mut rules = Vec::new();
        for e in effective {
            let Some(rule) = &e.rule else { continue };
            let runnable = match e.inactive_reason {
                None => true,
                Some(InactiveReason::NotRunnable) => false,
                Some(_) => continue,
            };
            if rule.input_action == InputAction::Off {
                continue;
            }
            let categories: Vec<Category> = rule.detector.categories.iter().copied().collect();
            let runner = match rule.detector.kind {
                DetectorKind::Pattern if runnable => {
                    PatternDetector::new(&categories, &rule.detector.patterns)
                        .map_or(Runner::CannotRun, Runner::Pattern)
                }
                _ => Runner::CannotRun,
            };
            rules.push(CompiledRule {
                id: e.id,
                rule_set_id: e.rule_set_id,
                layer: e.layer,
                revision: e.revision,
                kind: rule.detector.kind,
                categories,
                action: rule.input_action,
                on_error: rule.on_error,
                workloads: rule.workloads.clone(),
                uninspectable: rule.uninspectable,
                runner,
            });
        }
        Self { rules }
    }
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
    /// Whether any rule applies to `workload` (otherwise the request's text
    /// is not even collected).
    pub fn applies_to(&self, workload: Workload) -> bool {
        self.rules.iter().any(|r| r.workloads.contains(&workload))
    }
}

/// What a decision records (`guardrail_events.action`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Monitored,
    Blocked,
    DetectorFailed,
    Skipped,
}
impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitored => "monitored",
            Self::Blocked => "blocked",
            Self::DetectorFailed => "detector_failed",
            Self::Skipped => "skipped",
        }
    }
}

/// One recorded decision: a rule, a category, what happened and how many
/// distinct values were found (1 for a failure or a skip).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub rule_set_id: Uuid,
    pub rule_id: Uuid,
    pub revision: i64,
    pub layer: Layer,
    pub detector: DetectorKind,
    pub category: Category,
    pub action: Action,
    pub count: u32,
    /// The decision the client was refused with.
    pub refused: bool,
}

/// Why a request is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    /// A blocking rule found something (400 `guardrail_input_blocked`).
    InputBlocked,
    /// A fail-closed rule could not run (503 `guardrail_detector_failed`).
    DetectorFailed,
    /// An enforcing rule cannot read some of the request (400
    /// `guardrail_not_supported`).
    NotSupported,
}

/// The refusal a client sees: the rule set (policy), the rule, the category
/// family and the detector; ids and fixed codes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub rule_set_id: Uuid,
    pub rule_id: Uuid,
    pub category: Category,
    pub detector: DetectorKind,
}

impl Refusal {
    /// The `guardrail` object of an error body: the policy (rule set), the
    /// rule, the stage, the category family and the detector.
    pub fn json(&self) -> Value {
        serde_json::json!({
            "policy_id": self.rule_set_id,
            "rule_id": self.rule_id,
            "stage": "input",
            "category": self.category.as_str(),
            "detector": self.detector.as_str(),
        })
    }
}

/// The decision for one request.
#[derive(Default)]
pub struct Decision {
    pub events: Vec<Event>,
    pub refusal: Option<Refusal>,
}

fn rank(kind: RefusalKind) -> u8 {
    match kind {
        RefusalKind::InputBlocked => 0,
        RefusalKind::DetectorFailed => 1,
        RefusalKind::NotSupported => 2,
    }
}

impl Policy {
    /// Run every rule that applies to `workload` on `input` (`None`: the
    /// workload carries no text a detector can read).
    pub fn decide(&self, workload: Workload, input: Option<&Inspection<'_>>) -> Decision {
        let mut decision = Decision::default();
        let refuse = |d: &mut Decision, r: &CompiledRule, kind, category| {
            let refusal = Refusal {
                kind,
                rule_set_id: r.rule_set_id,
                rule_id: r.id,
                category,
                detector: r.kind,
            };
            if d.refusal.is_none_or(|c| rank(kind) < rank(c.kind)) {
                d.refusal = Some(refusal);
            }
        };
        for r in self
            .rules
            .iter()
            .filter(|r| r.workloads.contains(&workload))
        {
            let first = r.categories.first().copied().unwrap_or(Category::Custom);
            let event = |category, action, count| Event {
                rule_set_id: r.rule_set_id,
                rule_id: r.id,
                revision: r.revision,
                layer: r.layer,
                detector: r.kind,
                category,
                action,
                count,
                refused: false,
            };
            let Runner::Pattern(detector) = &r.runner else {
                decision
                    .events
                    .push(event(first, Action::DetectorFailed, 1));
                if r.on_error == OnError::FailClosed {
                    refuse(&mut decision, r, RefusalKind::DetectorFailed, first);
                }
                continue;
            };
            let blocking = r.action == InputAction::Block;
            if let Some(input) = input {
                let findings = detector.scan(input);
                for &category in &r.categories {
                    let count = findings.distinct(input, category);
                    if count == 0 {
                        continue;
                    }
                    let action = if blocking {
                        Action::Blocked
                    } else {
                        Action::Monitored
                    };
                    let count = u32::try_from(count).unwrap_or(u32::MAX).min(1_000_000);
                    decision.events.push(event(category, action, count));
                    if blocking {
                        refuse(&mut decision, r, RefusalKind::InputBlocked, category);
                    }
                }
            }
            if input.is_none_or(|i| i.uninspectable) {
                if blocking && r.uninspectable == Uninspectable::Deny {
                    decision.events.push(event(first, Action::Blocked, 1));
                    refuse(&mut decision, r, RefusalKind::NotSupported, first);
                } else {
                    decision.events.push(event(first, Action::Skipped, 1));
                }
            }
        }
        if let Some(r) = decision.refusal {
            let action = match r.kind {
                RefusalKind::InputBlocked | RefusalKind::NotSupported => Action::Blocked,
                RefusalKind::DetectorFailed => Action::DetectorFailed,
            };
            if let Some(e) = decision
                .events
                .iter_mut()
                .find(|e| e.rule_id == r.rule_id && e.category == r.category && e.action == action)
            {
                e.refused = true;
            }
        }
        decision
    }
}

/// The text a generation request sends upstream: every message, tool call
/// and tool definition, and the native Messages envelopes that replace them on
/// Anthropic routes (a value present in both counts once). Images, and
/// documents that are not plain text, cannot be read.
pub fn chat_inspection(request: &ChatRequest) -> Inspection<'_> {
    let mut input = Inspection::default();
    for m in &request.messages {
        let part = match m.role {
            Role::System | Role::Developer => Part::System,
            Role::Tool => Part::ToolResult,
            Role::User | Role::Assistant => Part::Content,
        };
        if let Some(text) = &m.content {
            input.push(text, part);
        }
        for call in &m.tool_calls {
            input.push(&call.name, Part::ToolArguments);
            input.push(&call.arguments, Part::ToolArguments);
        }
    }
    for tool in &request.tools {
        input.push(&tool.name, Part::ToolDescription);
        if let Some(d) = &tool.description {
            input.push(d, Part::ToolDescription);
        }
        walk(&tool.parameters, Part::ToolDescription, &mut input);
    }
    if let Some(options) = &request.messages_options {
        for v in &options.native_messages {
            walk(v, Part::Content, &mut input);
        }
        for v in &options.native_tools {
            walk(v, Part::ToolDescription, &mut input);
        }
        if let Some(system) = &options.system {
            walk(system, Part::System, &mut input);
        }
        if !options.images.is_empty() {
            input.uninspectable = true;
        }
    }
    input
}

/// The text of embedding inputs.
pub fn texts_inspection<'a>(texts: impl IntoIterator<Item = &'a String>) -> Inspection<'a> {
    let mut input = Inspection::default();
    for t in texts {
        input.push(t, Part::Content);
    }
    input
}

/// Every string of a JSON value, except encodings no detector reads: an
/// image, or a document whose source is not plain text, makes the request
/// uninspectable; signatures are opaque.
fn walk<'a>(v: &'a Value, part: Part, input: &mut Inspection<'a>) {
    match v {
        Value::String(s) => input.push(s, part),
        Value::Array(items) => items.iter().for_each(|i| walk(i, part, input)),
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str);
            if matches!(
                kind,
                Some("image" | "image_url" | "input_image" | "audio" | "file")
            ) {
                input.uninspectable = true;
                return;
            }
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("signature" | "type" | "role" | "media_type" | "cache_control", _) => {}
                    ("source", Value::Object(source)) => {
                        match source.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(Value::String(data)) = source.get("data") {
                                    input.push(data, part);
                                }
                            }
                            Some("content") => {
                                if let Some(content) = source.get("content") {
                                    walk(content, part, input);
                                }
                            }
                            // base64, url, file: encoded or remote content.
                            _ => input.uninspectable = true,
                        }
                    }
                    _ => walk(value, part, input),
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "enforce_tests.rs"]
mod tests;
