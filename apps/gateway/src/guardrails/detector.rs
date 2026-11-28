//! The detector interface (docs/guardrails-design.md §4): the text a request
//! carries, borrowed in segments, and what a detector found in it.
//!
//! Findings exist only in memory, for the request being inspected: a span
//! locates a match so the caller can count distinct values (and, in a later
//! phase, redact). Nothing here is stored or logged; a decision records the
//! rule, the category and a count.
use std::borrow::Cow;
use std::ops::Range;

use super::policy::{Category, DetectorKind, Egress, Kind};

/// Where a segment's text came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Content,
    ToolArguments,
    ToolResult,
    System,
    ToolDescription,
}

/// One piece of text the request sends upstream: borrowed from it, or
/// derived from it (parsed tool-call arguments, joined text blocks).
pub struct Segment<'a> {
    pub text: Cow<'a, str>,
    pub part: Part,
}

/// The text of one request, and whether it also carries parts no detector
/// can read (images, documents, audio). A text seen before (the canonical and
/// native forms of one message, say) is kept once.
#[derive(Default)]
pub struct Inspection<'a> {
    pub segments: Vec<Segment<'a>>,
    pub uninspectable: bool,
    /// Segments by the hash of their text. A hash already seen is compared
    /// with the texts it stands for, so a colliding text is never skipped.
    seen: std::collections::HashMap<u64, Vec<usize>>,
}
impl<'a> Inspection<'a> {
    fn fresh(&mut self, text: &str) -> bool {
        use std::hash::{BuildHasher, BuildHasherDefault};
        if text.is_empty() {
            return false;
        }
        let hash = BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
            .hash_one(text);
        let same = self.seen.get(&hash).is_some_and(|indices| {
            indices
                .iter()
                .any(|i| self.segments[*i].text.as_ref() == text)
        });
        if same {
            return false;
        }
        self.seen.entry(hash).or_default().push(self.segments.len());
        true
    }
    pub fn push(&mut self, text: &'a str, part: Part) {
        if self.fresh(text) {
            self.segments.push(Segment {
                text: Cow::Borrowed(text),
                part,
            });
        }
    }
    pub fn push_owned(&mut self, text: String, part: Part) {
        if self.fresh(&text) {
            self.segments.push(Segment {
                text: Cow::Owned(text),
                part,
            });
        }
    }
    /// Bytes of text to scan.
    pub fn len(&self) -> usize {
        self.segments.iter().map(|s| s.text.len()).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
    /// The same text, owned (to decide on the blocking pool).
    pub fn into_owned(self) -> Inspection<'static> {
        Inspection {
            segments: self
                .segments
                .into_iter()
                .map(|s| Segment {
                    text: Cow::Owned(s.text.into_owned()),
                    part: s.part,
                })
                .collect(),
            uninspectable: self.uninspectable,
            seen: self.seen,
        }
    }
}

/// One match: its category (and, for personal data, its kind), segment
/// index and byte span in that segment.
pub struct Finding {
    pub category: Category,
    pub kind: Option<Kind>,
    pub segment: usize,
    pub span: Range<usize>,
}

/// What a detector found; empty when nothing matched.
#[derive(Default)]
pub struct Findings(pub Vec<Finding>);
impl Findings {
    /// Distinct matched values of `category` (a value repeated in several
    /// places counts once). The values are compared in memory only.
    pub fn distinct(&self, input: &Inspection<'_>, category: Category) -> usize {
        self.distinct_where(input, |f| f.category == category)
    }
    /// Distinct matched values of personal-data `kind`.
    pub fn distinct_kind(&self, input: &Inspection<'_>, kind: Kind) -> usize {
        self.distinct_where(input, |f| f.kind == Some(kind))
    }
    fn distinct_where(&self, input: &Inspection<'_>, keep: impl Fn(&Finding) -> bool) -> usize {
        let mut seen = std::collections::HashSet::new();
        for f in self.0.iter().filter(|f| keep(f)) {
            if let Some(text) = input
                .segments
                .get(f.segment)
                .and_then(|s| s.text.as_ref().get(f.span.clone()))
            {
                seen.insert(text);
            }
        }
        seen.len()
    }
}

/// What one detector found in a request: distinct matched values per
/// category (per kind for personal data), and the category whose scan the
/// request's budget ran out in, if any (the counts found until then still
/// stand).
pub struct Inspected {
    pub counts: Vec<(Category, Option<Kind>, usize)>,
    pub spent: Option<Category>,
}

/// Why a detector gave no verdict at all (it cannot run in this release).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectorError {
    Unavailable,
}

/// The time (or, in tests, the number of regex calls) a request's custom
/// patterns may take in all. Checked before every call.
pub enum Budget {
    Clock(std::time::Instant),
    Steps(std::cell::Cell<u64>),
}
impl Budget {
    pub fn time(limit: std::time::Duration) -> Self {
        Self::Clock(std::time::Instant::now() + limit)
    }
    /// A budget of `n` regex calls, whatever they cost (tests).
    pub fn steps(n: u64) -> Self {
        Self::Steps(std::cell::Cell::new(n))
    }
    /// Take one step; `Err` once the budget is spent.
    pub fn check(&self) -> Result<(), BudgetSpent> {
        match self {
            Self::Clock(deadline) if std::time::Instant::now() >= *deadline => Err(BudgetSpent),
            Self::Clock(_) => Ok(()),
            Self::Steps(left) => match left.get() {
                0 => Err(BudgetSpent),
                n => {
                    left.set(n - 1);
                    Ok(())
                }
            },
        }
    }
    /// Steps left (a step budget; `None` for a clock).
    pub fn left(&self) -> Option<u64> {
        match self {
            Self::Steps(left) => Some(left.get()),
            Self::Clock(_) => None,
        }
    }
}
/// A request's custom-pattern budget ran out.
#[derive(Debug)]
pub struct BudgetSpent;

/// What a detector shares within one request: the budget, and the built-in
/// secret and personal-data findings (each scanned once for every rule that
/// uses them; personal data per kind, only for the kinds some rule names).
pub struct DetectContext<'a> {
    pub budget: &'a Budget,
    pub builtin: &'a std::cell::OnceCell<Findings>,
    /// Per kind: its findings, and whether the budget ran out before the
    /// end of its scan.
    pub personal: &'a std::cell::RefCell<std::collections::HashMap<Kind, (Findings, bool)>>,
}

/// Implemented by the built-in detectors (and, in later phases, model and
/// external ones, which add an asynchronous call). Phase 1's pattern detector
/// runs synchronously within the request's budget.
pub trait Detector: Send + Sync {
    fn kind(&self) -> DetectorKind;
    fn egress(&self) -> Egress;
    fn categories(&self) -> &[Category];
    fn inspect(
        &self,
        ctx: &DetectContext<'_>,
        input: &Inspection<'_>,
    ) -> Result<Inspected, DetectorError>;
}
