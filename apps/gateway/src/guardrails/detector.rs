//! The detector interface (docs/guardrails-design.md §4): the text a request
//! carries, borrowed in segments, and what a detector found in it.
//!
//! Findings exist only in memory, for the request being inspected: a span
//! locates a match so the caller can count distinct values (and, in a later
//! phase, redact). Nothing here is stored or logged; a decision records the
//! rule, the category and a count.
use std::borrow::Cow;
use std::ops::Range;

use async_trait::async_trait;
use tokio::time::Instant;

use super::policy::{Category, DetectorKind, Egress};

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
    seen: std::collections::HashSet<u64>,
    /// Keyed per process and inspection, so a client cannot compute a text
    /// whose hash equals another's and have it skipped.
    keys: std::collections::hash_map::RandomState,
}
impl<'a> Inspection<'a> {
    fn fresh(&mut self, text: &str) -> bool {
        use std::hash::BuildHasher;
        !text.is_empty() && self.seen.insert(self.keys.hash_one(text))
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
            keys: self.keys,
        }
    }
}

/// One match: its category, segment index and byte span in that segment.
pub struct Finding {
    pub category: Category,
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
        let mut seen = std::collections::HashSet::new();
        for f in self.0.iter().filter(|f| f.category == category) {
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

/// Why a detector gave no verdict. The rule's `on_error` decides what follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectorError {
    Timeout,
    Unavailable,
}

/// What a detector may know about the request beyond its text.
pub struct DetectContext {
    pub deadline: Instant,
}

/// Implemented by the built-in detectors and, later, external ones.
#[async_trait]
pub trait Detector: Send + Sync {
    fn kind(&self) -> DetectorKind;
    fn egress(&self) -> Egress;
    fn categories(&self) -> &[Category];
    async fn inspect(
        &self,
        ctx: &DetectContext,
        input: &Inspection<'_>,
    ) -> Result<Findings, DetectorError>;
}
