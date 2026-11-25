//! The detector interface (docs/guardrails-design.md §4): the text a request
//! carries, borrowed in segments, and what a detector found in it.
//!
//! Findings exist only in memory, for the request being inspected: a span
//! locates a match so the caller can count distinct values (and, in a later
//! phase, redact). Nothing here is stored or logged; a decision records the
//! rule, the category and a count.
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

/// One piece of text the request sends upstream.
pub struct Segment<'a> {
    pub text: &'a str,
    pub part: Part,
}

/// The text of one request, and whether it also carries parts no detector
/// can read (images, documents, audio).
#[derive(Default)]
pub struct Inspection<'a> {
    pub segments: Vec<Segment<'a>>,
    pub uninspectable: bool,
}
impl<'a> Inspection<'a> {
    pub fn push(&mut self, text: &'a str, part: Part) {
        if !text.is_empty() {
            self.segments.push(Segment { text, part });
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
                .and_then(|s| s.text.get(f.span.clone()))
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
