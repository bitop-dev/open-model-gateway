//! Compiled custom patterns shared by every workspace's policy on a replica
//! (docs/guardrails-design.md §14, "Cache"). A rule's detector is compiled
//! once per rule set revision and kept under (rule set, revision, rule): a
//! platform layer's patterns are compiled once, not once per cached
//! workspace, and concurrent loads of the same rule wait for that one
//! compile. A newer revision of a set evicts the older ones; the map is
//! bounded, and a policy keeps the detectors it uses whatever the map
//! evicts.
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::OnceCell;
use uuid::Uuid;

use super::limits::WAIT;
use super::patterns::{LOCAL_COMPILES, Limit, PLATFORM_COMPILES, in_slot_within};
use super::policy::{Category, DetectorKind, EffectiveRule, Layer};
use super::secrets::PatternDetector;

/// The compiled detector of one stored rule: `None` when its patterns no
/// longer compile (the rule cannot run).
pub type Compiled = Option<Arc<PatternDetector>>;

/// (rule set, revision, rule).
type Key = (Uuid, i64, Uuid);

/// Rules kept at most; beyond, an arbitrary eighth goes (their policies keep
/// what they hold).
const CAPACITY: usize = 4096;

type Cell = Arc<OnceCell<Compiled>>;
static SHARED: LazyLock<Mutex<HashMap<Key, Cell>>> = LazyLock::new(Default::default);

/// The cell of `key`, evicting older revisions of its set.
fn cell(key: Key) -> Cell {
    let mut map = SHARED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cell) = map.get(&key) {
        return Arc::clone(cell);
    }
    map.retain(|(set, revision, _), _| *set != key.0 || *revision >= key.1);
    if map.len() >= CAPACITY {
        let drop: Vec<Key> = map.keys().take(CAPACITY / 8 + 1).copied().collect();
        for k in drop {
            map.remove(&k);
        }
    }
    Arc::clone(map.entry(key).or_default())
}

/// No compile slot was free in time: the policy cannot be compiled now.
#[derive(Debug, PartialEq, Eq)]
pub struct NoSlot;

/// The compiled detectors of `effective`'s rules that have custom patterns,
/// by rule id: compiled once per replica (in a compile slot, waiting at most
/// [`WAIT`] for one), or reused.
pub async fn detectors(effective: &[EffectiveRule]) -> Result<HashMap<Uuid, Compiled>, NoSlot> {
    let mut out = HashMap::new();
    for e in effective {
        let Some(rule) = &e.rule else { continue };
        if e.inactive_reason.is_some()
            || rule.detector.kind != DetectorKind::Pattern
            || rule.detector.patterns.is_empty()
        {
            continue;
        }
        let categories: Vec<Category> = rule.detector.categories.iter().copied().collect();
        let patterns = rule.detector.patterns.clone();
        let limit = Limit::of(e.layer);
        let slots = if e.layer == Layer::WorkspaceLocal {
            &LOCAL_COMPILES
        } else {
            &PLATFORM_COMPILES
        };
        let set = e.rule_set_id;
        let cell = cell((set, e.revision, e.id));
        let compiled = cell
            .get_or_try_init(|| async move {
                in_slot_within(slots, Some(WAIT), move || {
                    #[cfg(test)]
                    counted(set);
                    PatternDetector::within(&categories, &patterns, limit).map(Arc::new)
                })
                .await
                .ok_or(NoSlot)
            })
            .await?;
        out.insert(e.id, compiled.clone());
    }
    Ok(out)
}

#[cfg(test)]
static COMPILES: LazyLock<Mutex<HashMap<Uuid, usize>>> = LazyLock::new(Default::default);
#[cfg(test)]
fn counted(set: Uuid) {
    *COMPILES.lock().unwrap().entry(set).or_default() += 1;
}
/// Rules of `set` compiled on this replica so far (tests).
#[cfg(test)]
pub fn compiles(set: Uuid) -> usize {
    COMPILES.lock().unwrap().get(&set).copied().unwrap_or(0)
}
