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
            // A rule this release cannot run fails under its on_error whatever
            // its input action (it may enforce on output); others run on input
            // only when their input action is not `off`.
            if runnable && rule.input_action == InputAction::Off {
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
        // The built-in secret shapes, scanned once for every rule that uses them.
        let mut builtin: Option<super::detector::Findings> = None;
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
                let custom = detector.custom_findings(input);
                for &category in &r.categories {
                    let count = if category == Category::Secret && detector.secrets() {
                        builtin
                            .get_or_insert_with(|| super::secrets::builtin(input))
                            .distinct(input, category)
                    } else {
                        custom.distinct(input, category)
                    };
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
                // Nothing was found: the rule could not read the request.
                decision.events.push(event(first, Action::Skipped, 1));
                if blocking && r.uninspectable == Uninspectable::Deny {
                    refuse(&mut decision, r, RefusalKind::NotSupported, first);
                }
            }
        }
        if let Some(r) = decision.refusal {
            let action = match r.kind {
                RefusalKind::InputBlocked => Action::Blocked,
                RefusalKind::NotSupported => Action::Skipped,
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

/// The text a generation request sends upstream: every message (content,
/// tool-call ids, names and arguments, raw and parsed), tool definitions, the
/// forced tool's name, and the native Messages envelopes and options that
/// replace or add to them on Anthropic routes (system, stop sequences,
/// output schema, context management, earlier thinking). A value present in
/// several forms counts once. Consecutive system messages, and a native
/// message's text blocks, are also read joined, so text split across blocks
/// is found. Images, audio, and documents that are not plain text cannot be
/// read.
pub fn chat_inspection(request: &ChatRequest) -> Inspection<'_> {
    let mut input = Inspection::default();
    let mut system = Vec::new();
    for m in &request.messages {
        let part = match m.role {
            Role::System | Role::Developer => Part::System,
            Role::Tool => Part::ToolResult,
            Role::User | Role::Assistant => Part::Content,
        };
        if let Some(text) = &m.content {
            input.push(text, part);
            if part == Part::System {
                system.push(text.as_str());
            }
        }
        if part != Part::System {
            join(&mut input, &mut system, Part::System);
        }
        if let Some(id) = &m.tool_call_id {
            input.push(id, Part::ToolResult);
        }
        for call in &m.tool_calls {
            input.push(&call.id, Part::ToolArguments);
            input.push(&call.name, Part::ToolArguments);
            input.push(&call.arguments, Part::ToolArguments);
            // Adapters may decode the arguments: read them decoded too.
            if let Ok(parsed) = serde_json::from_str::<Value>(&call.arguments) {
                owned(&parsed, Part::ToolArguments, &mut input);
            }
        }
    }
    join(&mut input, &mut system, Part::System);
    for tool in &request.tools {
        input.push(&tool.name, Part::ToolDescription);
        if let Some(d) = &tool.description {
            input.push(d, Part::ToolDescription);
        }
        json(&tool.parameters, Part::ToolDescription, &mut input);
    }
    if let Some(crate::inference::types::ToolChoice::Function(name)) = &request.tool_choice {
        input.push(name, Part::ToolDescription);
    }
    if let Some(options) = &request.messages_options {
        for m in &options.native_messages {
            message(m, &mut input);
        }
        for t in &options.native_tools {
            json(t, Part::ToolDescription, &mut input);
        }
        if let Some(system) = &options.system {
            content(system, Part::System, &mut input);
        }
        for t in &options.history_thinking {
            block(t, Part::Content, &mut input);
        }
        for v in [&options.output_config, &options.context_management]
            .into_iter()
            .flatten()
        {
            json(v, Part::System, &mut input);
        }
        for stop in options.stop_sequences.iter().flatten() {
            input.push(stop, Part::System);
        }
        if !options.images.is_empty() {
            input.uninspectable = true;
        }
    }
    input
}

/// Push the joined texts of `pending` (two or more) and clear it.
fn join<'a>(input: &mut Inspection<'a>, pending: &mut Vec<&'a str>, part: Part) {
    if pending.len() > 1 {
        input.push_owned(pending.concat(), part);
    }
    pending.clear();
}

/// The text of embedding inputs.
pub fn texts_inspection<'a>(texts: impl IntoIterator<Item = &'a String>) -> Inspection<'a> {
    let mut input = Inspection::default();
    for t in texts {
        input.push(t, Part::Content);
    }
    input
}

/// A native Messages message: `{role, content}`.
fn message<'a>(m: &'a Value, input: &mut Inspection<'a>) {
    match m.get("content") {
        Some(c) => content(c, Part::Content, input),
        None => json(m, Part::Content, input),
    }
}

/// Message or system content: a string, or blocks (whose texts are also
/// read joined).
fn content<'a>(c: &'a Value, part: Part, input: &mut Inspection<'a>) {
    match c {
        Value::String(s) => input.push(s, part),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            for b in blocks {
                if let Some(Value::String(t)) = b.get("text") {
                    texts.push(t.as_str());
                }
                block(b, part, input);
            }
            join(input, &mut texts, part);
        }
        other => json(other, part, input),
    }
}

/// One content block. Known shapes skip only what is opaque (a signature, an
/// encoded or remote source); any other value has every string read.
fn block<'a>(b: &'a Value, part: Part, input: &mut Inspection<'a>) {
    let Some(map) = b.as_object() else {
        return json(b, part, input);
    };
    match map.get("type").and_then(Value::as_str) {
        Some("image" | "image_url" | "input_image" | "audio" | "input_audio" | "file") => {
            input.uninspectable = true;
        }
        Some("thinking") => {
            if let Some(Value::String(t)) = map.get("thinking") {
                input.push(t, part);
            }
        }
        // Provider-encrypted reasoning the provider returned; it holds no
        // client text.
        Some("redacted_thinking") => {}
        Some("document") => {
            for (key, value) in map {
                if key == "source" {
                    source(value, part, input);
                } else {
                    json(value, part, input);
                }
            }
        }
        Some("tool_result") => {
            for (key, value) in map {
                if key == "content" {
                    content(value, Part::ToolResult, input);
                } else {
                    json(value, Part::ToolResult, input);
                }
            }
        }
        _ => json(b, part, input),
    }
}

/// A document's source: plain text or content blocks are read; anything
/// else (base64, a URL, a file) cannot be.
fn source<'a>(s: &'a Value, part: Part, input: &mut Inspection<'a>) {
    match s.get("type").and_then(Value::as_str) {
        Some("text") => match s.get("data") {
            Some(Value::String(data)) => input.push(data, part),
            _ => input.uninspectable = true,
        },
        Some("content") => match s.get("content") {
            Some(c) => content(c, part, input),
            None => input.uninspectable = true,
        },
        _ => input.uninspectable = true,
    }
}

/// Every string of an arbitrary JSON value (keys skip nothing).
fn json<'a>(v: &'a Value, part: Part, input: &mut Inspection<'a>) {
    match v {
        Value::String(s) => input.push(s, part),
        Value::Array(items) => items.iter().for_each(|i| json(i, part, input)),
        Value::Object(map) => map.values().for_each(|i| json(i, part, input)),
        _ => {}
    }
}

/// As [`json`], for a value the gateway parsed itself.
fn owned(v: &Value, part: Part, input: &mut Inspection<'_>) {
    match v {
        Value::String(s) => input.push_owned(s.clone(), part),
        Value::Array(items) => items.iter().for_each(|i| owned(i, part, input)),
        Value::Object(map) => map.values().for_each(|i| owned(i, part, input)),
        _ => {}
    }
}

#[cfg(test)]
#[path = "enforce_tests.rs"]
mod tests;
