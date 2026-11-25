//! Guardrail layer writes: the PUT wire format and what phase 1 accepts
//! (docs/guardrails-design.md §5); see [`super::policy`] for the model.
use super::policy::*;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use uuid::Uuid;
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

/// The body of a layer's PUT. `local_only` is required on platform layers and
/// refused on a local one. Only a JSON object: a positional array is refused.
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
    /// Required on platform layers, refused on a local one; null is refused.
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

    /// The first `limit` custom pattern strings, in body order (the order in
    /// which `validate` reaches them).
    pub fn patterns(&self, limit: usize) -> Vec<String> {
        self.rules
            .iter()
            .filter_map(|r| r.get("detector")?.get("patterns")?.as_array())
            .flatten()
            .filter_map(|p| Some(p.as_str()?.to_owned()))
            .take(limit)
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
    // A PUT replaces the whole layer, so a platform layer states `local_only`
    // every time: an omission never lifts an egress restriction by accident.
    let local_only = match (platform_layer, body.local_only) {
        (true, Some(Some(v))) => Some(v),
        (true, None) | (_, Some(None)) | (false, Some(Some(_))) => return Err(Refusal::Invalid),
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
