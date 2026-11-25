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

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// Most entries (rules and tightening entries) in one layer.
pub const MAX_RULES: usize = 50;
/// Most custom patterns in one layer, and their size.
pub const MAX_PATTERNS: usize = 50;
pub const MAX_PATTERN_BYTES: usize = 512;

macro_rules! names {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
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
names!(
    /// Ordered weakest to strictest.
    InputAction {
        Off = "off",
        Monitor = "monitor",
        Redact = "redact",
        Block = "block",
    }
);
names!(RedactScope {
    UserText = "user_text",
    LatestUserTurn = "latest_user_turn",
    All = "all",
});
names!(
    /// Ordered weakest to strictest.
    OutputMode {
        Off = "off",
        Monitor = "monitor",
        Stop = "stop",
        Block = "block",
    }
);
names!(
    /// Ordered weakest to strictest.
    OnError {
        FailOpen = "fail_open",
        FailClosed = "fail_closed",
    }
);
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
names!(
    /// Ordered weakest to strictest.
    Uninspectable {
        Allow = "allow",
        Deny = "deny",
    }
);

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

// --- Wire format -------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetectorIn {
    kind: String,
    categories: Vec<String>,
    #[serde(default, deserialize_with = "not_null")]
    patterns: Option<Vec<String>>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InputIn {
    #[serde(default, deserialize_with = "not_null")]
    action: Option<String>,
    #[serde(default, deserialize_with = "not_null")]
    redact_scope: Option<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct OutputIn {
    #[serde(default, deserialize_with = "not_null")]
    mode: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryIn {
    #[serde(default, deserialize_with = "not_null")]
    id: Option<Uuid>,
    #[serde(default, deserialize_with = "not_null")]
    tightens: Option<Uuid>,
    #[serde(default, deserialize_with = "not_null")]
    name: Option<String>,
    #[serde(default, deserialize_with = "not_null")]
    protection: Option<String>,
    #[serde(default, deserialize_with = "object")]
    detector: Option<DetectorIn>,
    #[serde(default, deserialize_with = "object")]
    input: Option<InputIn>,
    #[serde(default, deserialize_with = "object")]
    output: Option<OutputIn>,
    #[serde(default, deserialize_with = "supplied")]
    threshold: Option<Option<f64>>,
    #[serde(default, deserialize_with = "not_null")]
    on_error: Option<String>,
    #[serde(default, deserialize_with = "supplied")]
    timeout_ms: Option<Option<u32>>,
    #[serde(default, deserialize_with = "not_null")]
    workloads: Option<Vec<String>>,
    #[serde(default, deserialize_with = "not_null")]
    uninspectable: Option<String>,
}
/// An optional object field: present means a JSON object (never null, never a
/// positional array, which serde would otherwise read into a struct).
fn object<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(
    d: D,
) -> Result<Option<T>, D::Error> {
    let map = serde_json::Map::<String, Value>::deserialize(d)?;
    T::deserialize(Value::Object(map))
        .map(Some)
        .map_err(serde::de::Error::custom)
}
/// An optional field that, when present, is not null (absent is `None`).
fn not_null<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)?
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("null where a value is required"))
}
fn supplied<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// The body of a layer's PUT. `local_only` only on platform layers. Only a
/// JSON object: a positional array is refused.
pub struct LayerIn {
    local_only: Option<Option<bool>>,
    rules: Vec<Value>,
}
impl<'de> Deserialize<'de> for LayerIn {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let map = serde_json::Map::<String, Value>::deserialize(d)?;
        let fields =
            LayerFields::deserialize(Value::Object(map)).map_err(serde::de::Error::custom)?;
        Ok(Self {
            local_only: fields.local_only,
            rules: fields.rules,
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerFields {
    /// Absent is false; null is refused.
    #[serde(default, deserialize_with = "supplied")]
    local_only: Option<Option<bool>>,
    rules: Vec<Value>,
}

impl LayerIn {
    /// The ids the body names (to tell its own rules from other layers').
    pub fn ids(&self) -> Vec<Uuid> {
        self.rules
            .iter()
            .filter_map(|r| r.get("id")?.as_str()?.parse().ok())
            .collect()
    }
}

fn name_of<T: Copy>(parse: fn(&str) -> Option<T>, v: &str) -> Result<T, Refusal> {
    parse(v).ok_or(Refusal::Invalid)
}
fn valid_name(n: &str) -> bool {
    let len = n.chars().count();
    (1..=100).contains(&len) && !n.trim().is_empty() && !n.chars().any(char::is_control)
}
fn threshold(v: f64) -> Result<Threshold, Refusal> {
    let t = (v * 1000.0).round();
    if !(0.0..=1.0).contains(&v) || ((v * 1000.0) - t).abs() > 1e-6 {
        return Err(Refusal::Invalid);
    }
    Ok(t as Threshold)
}
fn needs<T: PartialEq>(allowed: &[T], v: &T) -> Result<(), Refusal> {
    if allowed.contains(v) {
        Ok(())
    } else {
        Err(Refusal::DetectorUnavailable)
    }
}

fn parse_rule(e: EntryIn) -> Result<Rule, Refusal> {
    let name = e.name.filter(|n| valid_name(n)).ok_or(Refusal::Invalid)?;
    let protection = name_of(
        Protection::parse,
        e.protection.as_deref().ok_or(Refusal::Invalid)?,
    )?;
    let d = e.detector.ok_or(Refusal::Invalid)?;
    let kind = name_of(DetectorKind::parse, &d.kind)?;
    let categories = d
        .categories
        .iter()
        .map(|c| name_of(Category::parse, c))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if categories.is_empty() || categories.len() != d.categories.len() {
        return Err(Refusal::Invalid);
    }
    let input = e.input.unwrap_or_default();
    let input_action = name_of(
        InputAction::parse,
        input.action.as_deref().ok_or(Refusal::Invalid)?,
    )?;
    let redact_scope = input
        .redact_scope
        .as_deref()
        .map_or(Ok(RedactScope::UserText), |v| {
            name_of(RedactScope::parse, v)
        })?;
    let output_mode = e
        .output
        .and_then(|o| o.mode)
        .as_deref()
        .map_or(Ok(OutputMode::Off), |v| name_of(OutputMode::parse, v))?;
    let on_error = e.on_error.as_deref().map_or(
        // Block and redact rules fail closed, monitor rules open (§4).
        Ok(
            if input_action >= InputAction::Redact || output_mode >= OutputMode::Stop {
                OnError::FailClosed
            } else {
                OnError::FailOpen
            },
        ),
        |v| name_of(OnError::parse, v),
    )?;
    let workloads = match e.workloads {
        None => BTreeSet::from([Workload::Generation, Workload::Embeddings]),
        Some(list) => {
            let set = list
                .iter()
                .map(|w| name_of(Workload::parse, w))
                .collect::<Result<BTreeSet<_>, _>>()?;
            if set.is_empty() || set.len() != list.len() {
                return Err(Refusal::Invalid);
            }
            set
        }
    };
    let uninspectable = e
        .uninspectable
        .as_deref()
        .map_or(Ok(Uninspectable::Deny), |v| {
            name_of(Uninspectable::parse, v)
        })?;
    let threshold = e.threshold.flatten().map(threshold).transpose()?;
    let timeout_ms = e.timeout_ms.flatten();
    if timeout_ms.is_some_and(|t| !(1..=30_000).contains(&t)) {
        return Err(Refusal::Invalid);
    }
    let patterns = d.patterns.unwrap_or_default();
    if categories.contains(&Category::Custom) != !patterns.is_empty() {
        // Custom rules carry their patterns; built-in categories carry none.
        return Err(Refusal::Invalid);
    }
    if patterns.len() > MAX_PATTERNS
        || patterns
            .iter()
            .any(|p| p.is_empty() || p.len() > MAX_PATTERN_BYTES)
    {
        return Err(Refusal::Invalid);
    }
    if kind == DetectorKind::Pattern && (threshold.is_some() || timeout_ms.is_some()) {
        // The pattern detector neither scores nor waits.
        return Err(Refusal::Invalid);
    }
    Ok(Rule {
        name,
        protection,
        detector: Detector {
            kind,
            categories,
            patterns,
            reference: None,
        },
        input_action,
        redact_scope,
        output_mode,
        threshold,
        on_error,
        timeout_ms,
        workloads,
        uninspectable,
    })
}

fn parse_tightening(e: &EntryIn) -> Result<Tightening, Refusal> {
    if e.name.is_some()
        || e.protection.is_some()
        || e.detector.is_some()
        || e.timeout_ms.is_some()
        || e.workloads.is_some()
        || e.uninspectable.is_some()
        || e.input.as_ref().is_some_and(|i| i.redact_scope.is_some())
    {
        // A tightening entry names only what it tightens.
        return Err(Refusal::Invalid);
    }
    let t = Tightening {
        input_action: e
            .input
            .as_ref()
            .and_then(|i| i.action.as_deref())
            .map(|v| name_of(InputAction::parse, v))
            .transpose()?,
        output_mode: e
            .output
            .as_ref()
            .and_then(|o| o.mode.as_deref())
            .map(|v| name_of(OutputMode::parse, v))
            .transpose()?,
        on_error: e
            .on_error
            .as_deref()
            .map(|v| name_of(OnError::parse, v))
            .transpose()?,
        threshold: e.threshold.flatten().map(threshold).transpose()?,
    };
    if t == Tightening::default() {
        return Err(Refusal::Invalid);
    }
    Ok(t)
}

/// The refusal of a rule this release cannot run (after the egress check).
fn availability(r: &Rule) -> Result<(), Refusal> {
    needs(available::DETECTORS, &r.detector.kind)?;
    needs(available::PROTECTIONS, &r.protection)?;
    for c in &r.detector.categories {
        needs(available::CATEGORIES, c)?;
    }
    needs(available::INPUT_ACTIONS, &r.input_action)?;
    needs(available::OUTPUT_MODES, &r.output_mode)?;
    for w in &r.workloads {
        needs(available::WORKLOADS, w)?;
    }
    needs(available::UNINSPECTABLE, &r.uninspectable)
}

/// What a write is checked against.
pub struct WriteContext<'a> {
    pub layer: Layer,
    /// The workspace's current platform layer (local writes only).
    pub platform: Option<&'a RuleSet>,
    /// The layer's own current entries, whose ids a PUT may keep.
    pub current: Option<&'a RuleSet>,
    /// Ids of rules in other layers, which a PUT may not take over.
    pub foreign_ids: &'a HashSet<Uuid>,
    /// Compiles a custom pattern within the limits.
    pub compile: &'a dyn Fn(&str) -> bool,
}

/// Validate a layer's PUT body into its entries (new ids for new entries) and
/// `local_only`.
pub fn validate(
    body: LayerIn,
    cx: &WriteContext<'_>,
) -> Result<(Option<bool>, Vec<StoredRule>), Refusal> {
    let platform_layer = cx.layer != Layer::WorkspaceLocal;
    let local_only = match (platform_layer, body.local_only) {
        (true, None) => Some(false),
        (true, Some(Some(v))) => Some(v),
        (_, Some(None)) | (false, Some(Some(_))) => return Err(Refusal::Invalid),
        (false, None) => None,
    };
    if body.rules.len() > MAX_RULES {
        return Err(Refusal::Invalid);
    }
    // Under a platform's `local_only`, no rule may send text out.
    let egress_closed = if platform_layer {
        local_only == Some(true)
    } else {
        cx.platform.and_then(|p| p.local_only) == Some(true)
    };
    let own: HashSet<Uuid> = cx
        .current
        .map(|s| s.rules.iter().map(|r| r.id).collect())
        .unwrap_or_default();
    let platform_rules: BTreeMap<Uuid, &Rule> = cx
        .platform
        .map(|p| {
            p.rules
                .iter()
                .filter_map(|r| match &r.entry {
                    Entry::Rule(rule) => Some((r.id, rule.as_ref())),
                    Entry::Tightens(..) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut tightened = HashSet::new();
    let mut patterns = 0usize;
    let mut out = Vec::with_capacity(body.rules.len());
    for raw in body.rules {
        if !raw.is_object() {
            return Err(Refusal::Invalid);
        }
        let e: EntryIn = serde_json::from_value(raw).map_err(|_| Refusal::Invalid)?;
        let id = match e.id {
            Some(id) if own.contains(&id) => id,
            Some(id) if platform_rules.contains_key(&id) => {
                return Err(Refusal::LoosenNotAllowed(id));
            }
            Some(id) if cx.foreign_ids.contains(&id) => return Err(Refusal::Invalid),
            Some(_) => return Err(Refusal::Invalid),
            None => Uuid::now_v7(),
        };
        if !seen.insert(id) {
            return Err(Refusal::Invalid);
        }
        let entry = match e.tightens {
            Some(target) => {
                if platform_layer {
                    return Err(Refusal::Invalid);
                }
                let t = parse_tightening(&e)?;
                let base = platform_rules.get(&target).ok_or(Refusal::Invalid)?;
                if !tightened.insert(target) {
                    return Err(Refusal::Invalid);
                }
                if t.input_action
                    .is_some_and(|a| !available::INPUT_ACTIONS.contains(&a))
                    || t.output_mode
                        .is_some_and(|m| !available::OUTPUT_MODES.contains(&m))
                {
                    return Err(Refusal::DetectorUnavailable);
                }
                if t.threshold.is_some() && base.threshold.is_none() {
                    // A threshold for a rule whose detector does not score.
                    return Err(Refusal::Invalid);
                }
                if loosens(base, &t) {
                    return Err(Refusal::LoosenNotAllowed(target));
                }
                Entry::Tightens(target, t)
            }
            None => {
                let rule = parse_rule(e)?;
                if egress_closed && rule.detector.kind.egress() != Egress::Local {
                    return Err(Refusal::EgressNotAllowed);
                }
                availability(&rule)?;
                patterns += rule.detector.patterns.len();
                if patterns > MAX_PATTERNS {
                    return Err(Refusal::Invalid);
                }
                if !rule.detector.patterns.iter().all(|p| (cx.compile)(p)) {
                    return Err(Refusal::PatternInvalid);
                }
                Entry::Rule(Box::new(rule))
            }
        };
        out.push(StoredRule { id, entry });
    }
    Ok((local_only, out))
}

/// Whether a tightening entry would make its platform rule weaker anywhere.
fn loosens(base: &Rule, t: &Tightening) -> bool {
    t.input_action.is_some_and(|a| a < base.input_action)
        || t.output_mode.is_some_and(|m| m < base.output_mode)
        || t.on_error.is_some_and(|o| o < base.on_error)
        || match (t.threshold, base.threshold) {
            (Some(_), None) => true,
            (Some(a), Some(b)) => a > b,
            (None, _) => false,
        }
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

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
