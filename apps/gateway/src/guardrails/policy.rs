//! The guardrail policy model (docs/guardrails-design.md §5): rules, layers,
//! what phase 1 accepts on write, and tighten-only composition.
//!
//! A rule set belongs to one layer: the workspace-type default, a platform
//! per-workspace override that replaces it, or the workspace's local layer.
//! The effective policy is the platform layer's rules (the override when one
//! exists, else the type default) plus the local layer's rules. Every rule runs
//! on its own and the strictest outcome wins, so a local rule can only add
//! protection. A local *tightening entry* names a platform rule and makes some
//! of its fields stricter; composition takes the stricter of the two values for
//! each field, so an entry written against an older platform rule never makes
//! the current one weaker. Nothing is dropped silently: a rule that cannot run
//! is listed with an `inactive_reason`.
use std::collections::{BTreeMap, BTreeSet, HashSet};

use uuid::Uuid;

pub use super::view::{detectors_json, effective_json, rule_json};
pub use super::write::{LayerIn, WriteContext, validate};

/// Most entries (rules and tightening entries) in one layer.
pub const MAX_RULES: usize = 50;
/// Most custom patterns in one layer, and their size.
pub const MAX_PATTERNS: usize = 50;
pub const MAX_PATTERN_BYTES: usize = 512;

macro_rules! names {
    ($name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub fn parse(value: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.as_str() == value)
            }
        }
    };
}

names!(Layer {
    TypeDefault = "type_default",
    WorkspaceOverride = "workspace_override",
    WorkspaceLocal = "workspace_local",
});
names!(DetectorKind {
    Pattern = "pattern",
    Systemone = "systemone",
    LlmJudge = "llm_judge",
    External = "external",
});
names!(Protection {
    Secrets = "secrets",
    PersonalData = "personal_data",
    Moderation = "moderation",
    OutputDlp = "output_dlp",
    PromptInjection = "prompt_injection",
    Custom = "custom",
});
names!(Category {
    Secret = "secret",
    PersonalData = "personal_data",
    Moderation = "moderation",
    PromptInjection = "prompt_injection",
    Custom = "custom",
});
// Ordered weakest to strictest.
names!(InputAction {
    Off = "off",
    Monitor = "monitor",
    Redact = "redact",
    Block = "block",
});
names!(RedactScope {
    UserText = "user_text",
    LatestUserTurn = "latest_user_turn",
    All = "all",
});
// Ordered weakest to strictest.
names!(OutputMode {
    Off = "off",
    Monitor = "monitor",
    Stop = "stop",
    Block = "block",
});
// Ordered weakest to strictest.
names!(OnError {
    FailOpen = "fail_open",
    FailClosed = "fail_closed",
});
names!(Workload {
    Generation = "generation",
    Embeddings = "embeddings",
    Rerank = "rerank",
    Systemone = "systemone",
    AudioSpeech = "audio_speech",
    Images = "images",
    AudioTranscriptions = "audio_transcriptions",
    Realtime = "realtime",
});
// Ordered weakest to strictest.
names!(Uninspectable {
    Allow = "allow",
    Deny = "deny",
});

/// Where a detector sends the text it inspects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Egress {
    /// Nothing leaves the gateway.
    Local,
    /// A model the gateway serves.
    Model,
    /// An external service.
    Service,
}
impl Egress {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Model => "model",
            Self::Service => "service",
        }
    }
}
impl DetectorKind {
    pub fn egress(self) -> Egress {
        match self {
            Self::Pattern => Egress::Local,
            Self::Systemone | Self::LlmJudge => Egress::Model,
            Self::External => Egress::Service,
        }
    }
}

/// What this release can enforce (phase 1, #840): the pattern detector, on
/// input only, for secrets and custom patterns, with monitor and block, for
/// generation and embeddings, refusing uninspectable content. A rule that needs
/// more is refused on write (`guardrail_detector_unavailable`), never stored and
/// ignored.
pub mod available {
    use super::*;
    pub const DETECTORS: &[DetectorKind] = &[DetectorKind::Pattern];
    pub const PROTECTIONS: &[Protection] = &[Protection::Secrets, Protection::Custom];
    pub const CATEGORIES: &[Category] = &[Category::Secret, Category::Custom];
    pub const INPUT_ACTIONS: &[InputAction] =
        &[InputAction::Off, InputAction::Monitor, InputAction::Block];
    pub const OUTPUT_MODES: &[OutputMode] = &[OutputMode::Off];
    pub const WORKLOADS: &[Workload] = &[Workload::Generation, Workload::Embeddings];
    pub const UNINSPECTABLE: &[Uninspectable] = &[Uninspectable::Deny];
}

/// A threshold in thousandths (0..=1000), for detectors that score.
pub type Threshold = u16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detector {
    pub kind: DetectorKind,
    pub categories: BTreeSet<Category>,
    /// Custom patterns (category `custom` only): configuration, never matched text.
    pub patterns: Vec<String>,
    /// A later phase's detector model or service.
    pub reference: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub protection: Protection,
    pub detector: Detector,
    pub input_action: InputAction,
    pub redact_scope: RedactScope,
    pub output_mode: OutputMode,
    pub threshold: Option<Threshold>,
    pub on_error: OnError,
    pub timeout_ms: Option<u32>,
    pub workloads: BTreeSet<Workload>,
    pub uninspectable: Uninspectable,
}

/// A local entry that makes a platform rule stricter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tightening {
    pub input_action: Option<InputAction>,
    pub output_mode: Option<OutputMode>,
    pub on_error: Option<OnError>,
    pub threshold: Option<Threshold>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Rule(Box<Rule>),
    Tightens(Uuid, Tightening),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRule {
    pub id: Uuid,
    pub entry: Entry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSet {
    pub id: Uuid,
    pub layer: Layer,
    pub revision: i64,
    /// Platform layers only.
    pub local_only: Option<bool>,
    pub rules: Vec<StoredRule>,
}

/// Why a write is refused. Each maps to one management error; none carries a
/// pattern or any other content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A malformed body (plain 400).
    Invalid,
    /// 400 `guardrail_detector_unavailable`: needs a later phase.
    DetectorUnavailable,
    /// 403 `guardrail_egress_not_allowed`.
    EgressNotAllowed,
    /// 403 `guardrail_loosen_not_allowed`, naming the platform rule.
    LoosenNotAllowed(Uuid),
    /// 400 `guardrail_pattern_invalid`: does not compile within the limits.
    PatternInvalid,
}

/// The rule with a tightening entry applied: the stricter value of each field.
pub fn tighten(base: &Rule, t: &Tightening) -> Rule {
    let mut r = base.clone();
    if let Some(a) = t.input_action {
        r.input_action = r.input_action.max(a);
    }
    if let Some(m) = t.output_mode {
        r.output_mode = r.output_mode.max(m);
    }
    if let Some(o) = t.on_error {
        r.on_error = r.on_error.max(o);
    }
    if let (Some(a), Some(b)) = (t.threshold, r.threshold) {
        r.threshold = Some(a.min(b));
    }
    r
}
// --- Composition -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InactiveReason {
    /// The platform's `local_only` forbids this rule's detector.
    EgressNotAllowed,
    /// A tightening entry whose platform rule no longer exists.
    TightenedRuleMissing,
}
impl InactiveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EgressNotAllowed => "egress_not_allowed",
            Self::TightenedRuleMissing => "tightened_rule_missing",
        }
    }
}

/// One entry of the effective policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveRule {
    pub id: Uuid,
    pub rule_set_id: Uuid,
    pub layer: Layer,
    pub revision: i64,
    /// The rule as it runs (tightened), or `None` for a dangling tightening entry.
    pub rule: Option<Rule>,
    /// A dangling tightening entry's target.
    pub tightens: Option<Uuid>,
    /// Local entries that tighten this rule.
    pub tightened_by: Vec<Uuid>,
    pub inactive_reason: Option<InactiveReason>,
}
impl EffectiveRule {
    /// Whether this entry runs.
    pub fn active(&self) -> bool {
        self.rule.is_some() && self.inactive_reason.is_none()
    }
}

/// The effective policy of a workspace: its platform layer, then its local
/// layer. Inactive entries are kept, with their reason.
pub fn compose(platform: Option<&RuleSet>, local: Option<&RuleSet>) -> Vec<EffectiveRule> {
    let egress_closed = platform.and_then(|p| p.local_only) == Some(true);
    let mut tightenings: BTreeMap<Uuid, Vec<(Uuid, &Tightening)>> = BTreeMap::new();
    let platform_ids: HashSet<Uuid> = platform
        .map(|p| p.rules.iter().map(|r| r.id).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut dangling = Vec::new();
    if let Some(l) = local {
        for r in &l.rules {
            if let Entry::Tightens(target, t) = &r.entry {
                if platform_ids.contains(target) {
                    tightenings.entry(*target).or_default().push((r.id, t));
                } else {
                    dangling.push(EffectiveRule {
                        id: r.id,
                        rule_set_id: l.id,
                        layer: l.layer,
                        revision: l.revision,
                        rule: None,
                        tightens: Some(*target),
                        tightened_by: Vec::new(),
                        inactive_reason: Some(InactiveReason::TightenedRuleMissing),
                    });
                }
            }
        }
    }
    let mut push = |set: &RuleSet, id: Uuid, rule: &Rule, by: Vec<(Uuid, &Tightening)>| {
        let rule = by.iter().fold(rule.clone(), |r, (_, t)| tighten(&r, t));
        let inactive = (egress_closed && rule.detector.kind.egress() != Egress::Local)
            .then_some(InactiveReason::EgressNotAllowed);
        out.push(EffectiveRule {
            id,
            rule_set_id: set.id,
            layer: set.layer,
            revision: set.revision,
            rule: Some(rule),
            tightens: None,
            tightened_by: by.iter().map(|(id, _)| *id).collect(),
            inactive_reason: inactive,
        });
    };
    for set in [platform, local].into_iter().flatten() {
        for r in &set.rules {
            if let Entry::Rule(rule) = &r.entry {
                let by = tightenings.remove(&r.id).unwrap_or_default();
                push(set, r.id, rule, by);
            }
        }
    }
    out.extend(dangling);
    out
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
