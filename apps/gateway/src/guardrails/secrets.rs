//! The pattern detector (docs/guardrails-design.md §4): the built-in secret
//! rules and a rule's custom patterns, on the `regex` crate (linear time, no
//! backtracking). Nothing leaves the gateway.
//!
//! The built-in rules match credentials by their documented shape: a fixed
//! prefix or structure, so ordinary code, UUIDs, hashes and base64 data do not
//! match. A match counts unless it is glued to a run of eight or more
//! base64url characters (letters, digits, `_`, `-`) on either side: inside an
//! encoded blob it is data, not a credential. Quotes, spaces, `=`, `:` and
//! escapes such as `\n` or `%20` around one do not hide it. A PEM private key is matched on its `BEGIN` line, which is enough to
//! refuse it and keeps the match short.
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::{Regex, RegexSet};

use super::detector::{DetectContext, Detector, DetectorError, Finding, Findings, Inspection};
use super::policy::{Category, DetectorKind, Egress};

/// The built-in secret shapes, by name (names are for tests and docs only;
/// a decision records the rule and the category `secret`).
pub const BUILTIN: &[(&str, &str)] = &[
    ("aws_access_key_id", r"(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}"),
    (
        "github_token",
        r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}",
    ),
    ("github_fine_grained_token", r"github_pat_[A-Za-z0-9_]{82}"),
    ("gitlab_token", r"glpat-[A-Za-z0-9_\-]{20,}"),
    ("slack_token", r"xox[abposr]-[A-Za-z0-9-]{10,250}"),
    (
        "slack_webhook",
        r"https://hooks\.slack\.com/services/T[A-Z0-9]{8,}/B[A-Z0-9]{8,}/[A-Za-z0-9]{24}",
    ),
    ("stripe_secret_key", r"(?:sk|rk)_live_[A-Za-z0-9]{24,99}"),
    (
        "openai_api_key",
        r"sk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}T3BlbkFJ[A-Za-z0-9_\-]{20,}",
    ),
    (
        "anthropic_api_key",
        r"sk-ant-(?:api|admin)[0-9]{2}-[A-Za-z0-9_\-]{80,120}",
    ),
    ("google_api_key", r"AIza[0-9A-Za-z_\-]{35}"),
    ("huggingface_token", r"hf_[A-Za-z0-9]{34,40}"),
    ("npm_token", r"npm_[A-Za-z0-9]{36}"),
    ("private_key", r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"),
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{16,}",
    ),
    (
        "connection_string_password",
        r"(?:postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|rediss?|amqps?|mssql|sqlserver)://[^\s:/@]{1,256}:[^\s@/]{1,256}@[^\s/]{1,256}",
    ),
    (
        "azure_storage_connection_string",
        r"AccountKey=[A-Za-z0-9+/]{80,}={0,2}",
    ),
    (
        "gateway_inference_key",
        r"omg_[0-9a-fA-F]{32}\.[0-9a-fA-F]{64}",
    ),
    (
        "gateway_admin_token",
        r"omg_admin_[0-9a-fA-F]{32}\.[0-9a-fA-F]{64}",
    ),
];

struct Builtin {
    any: RegexSet,
    each: Vec<Regex>,
}
static SECRETS: LazyLock<Builtin> = LazyLock::new(|| Builtin {
    any: RegexSet::new(BUILTIN.iter().map(|(_, p)| *p)).expect("built-in secret patterns"),
    each: BUILTIN
        .iter()
        .map(|(_, p)| Regex::new(p).expect("built-in secret pattern"))
        .collect(),
});

/// At most this many matches are kept per scan (the count saturates there),
/// so a pattern that matches everywhere costs bounded memory.
pub const MAX_FINDINGS: usize = 10_000;

fn encoded(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
/// A run of this many base64url characters next to a match makes it part of
/// an encoded blob.
const BLOB_RUN: usize = 8;
/// Whether `span` of `text` stands apart from base64url data around it.
fn standalone(text: &[u8], span: std::ops::Range<usize>) -> bool {
    let before = text[..span.start]
        .iter()
        .rev()
        .take(BLOB_RUN)
        .take_while(|b| encoded(**b))
        .count();
    let after = text[span.end..]
        .iter()
        .take(BLOB_RUN)
        .take_while(|b| encoded(**b))
        .count();
    before < BLOB_RUN && after < BLOB_RUN
}

/// Every match of the built-in secret shapes (shared by every rule that has
/// the `secret` category in one request).
pub fn builtin(input: &Inspection<'_>) -> Findings {
    let mut found = Vec::new();
    for (index, segment) in input.segments.iter().enumerate() {
        for i in SECRETS.any.matches(&segment.text).iter() {
            for m in SECRETS.each[i]
                .find_iter(&segment.text)
                .filter(|m| standalone(segment.text.as_bytes(), m.range()))
            {
                if found.len() == MAX_FINDINGS {
                    return Findings(found);
                }
                found.push(Finding {
                    category: Category::Secret,
                    segment: index,
                    span: m.range(),
                });
            }
        }
    }
    Findings(found)
}

/// One rule's pattern detector: the built-in secret shapes when the rule has
/// the `secret` category, and its custom patterns (compiled within the limits
/// of `patterns::compile`) for `custom`.
pub struct PatternDetector {
    categories: Vec<Category>,
    secrets: bool,
    custom: Vec<Regex>,
}
impl PatternDetector {
    /// `None` when a custom pattern no longer compiles within the limits (the
    /// rule then fails like a detector that cannot run).
    pub fn new(categories: &[Category], patterns: &[String]) -> Option<Self> {
        Self::within(categories, patterns, super::patterns::Limit::Local)
    }
    /// As [`PatternDetector::new`], for patterns of a layer with `limit`.
    pub fn within(
        categories: &[Category],
        patterns: &[String],
        limit: super::patterns::Limit,
    ) -> Option<Self> {
        let custom = patterns
            .iter()
            .map(|p| super::patterns::compile_within(p, limit))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            categories: categories.to_vec(),
            secrets: categories.contains(&Category::Secret),
            custom,
        })
    }
    /// Every match, synchronously (the detector never waits on anything);
    /// `BudgetSpent` when the custom patterns ran out of time.
    pub fn scan(&self, input: &Inspection<'_>) -> Result<Findings, BudgetSpent> {
        let mut found = if self.secrets {
            builtin(input).0
        } else {
            Vec::new()
        };
        let budget = Budget::new(super::enforce::CUSTOM_BUDGET);
        found.extend(self.custom_findings(input, &budget)?.0);
        Ok(Findings(found))
    }
    /// Whether the rule has custom patterns.
    pub fn has_custom(&self) -> bool {
        !self.custom.is_empty()
    }
    /// Whether the built-in secret shapes run for this rule.
    pub fn secrets(&self) -> bool {
        self.secrets
    }
    /// Matches of the rule's custom patterns, within a time budget: a
    /// custom pattern may be any user's, so its cost per byte is too. Each
    /// step searches the text from its position, with the text before it as
    /// context (so `\b` and `^` mean what they mean in the whole text), but
    /// only up to `WINDOW` bytes ahead, so one regex call is bounded; the
    /// budget is checked before every call, and spent, the scan stops with
    /// `BudgetSpent` (the rule then fails under its `on_error`). A match cut
    /// by that horizon is searched again up to `EXTEND` bytes further and
    /// counted. A match that needs more than `OVERLAP` bytes past the horizon
    /// to start matching at all can be missed; empty matches never count.
    pub fn custom_findings(
        &self,
        input: &Inspection<'_>,
        budget: &Budget,
    ) -> Result<Findings, BudgetSpent> {
        let mut found = Vec::new();
        for (index, segment) in input.segments.iter().enumerate() {
            let text: &str = &segment.text;
            for pattern in &self.custom {
                let mut pos = 0;
                while pos < text.len() {
                    budget.check()?;
                    let end = boundary(text, (pos + WINDOW).min(text.len()));
                    let Some(mut m) = pattern.find_at(&text[..end], pos) else {
                        if end == text.len() {
                            break;
                        }
                        let back = boundary(text, end - OVERLAP.min(end - pos));
                        pos = boundary_up(text, back.max(pos + 1));
                        continue;
                    };
                    if m.end() == end && end < text.len() {
                        budget.check()?;
                        let further = boundary(text, (m.start() + EXTEND).min(text.len()));
                        if let Some(longer) = pattern.find_at(&text[..further], m.start()) {
                            m = longer;
                        }
                    }
                    if !m.is_empty() {
                        if found.len() == MAX_FINDINGS {
                            return Ok(Findings(found));
                        }
                        found.push(Finding {
                            category: Category::Custom,
                            segment: index,
                            span: m.range(),
                        });
                    }
                    pos = if m.end() > pos {
                        m.end()
                    } else {
                        boundary_up(text, pos + 1)
                    };
                }
            }
        }
        Ok(Findings(found))
    }
}

/// How far ahead one custom-pattern call looks; after a window without a
/// match the next starts `OVERLAP` bytes before its end; a match cut by the
/// window is searched again up to `EXTEND` bytes from its start.
const WINDOW: usize = 4096;
const OVERLAP: usize = 1024;
const EXTEND: usize = 8192;

/// The time a request's custom patterns may take in all.
pub struct Budget(std::time::Instant);
impl Budget {
    pub fn new(limit: std::time::Duration) -> Self {
        Self(std::time::Instant::now() + limit)
    }
    fn check(&self) -> Result<(), BudgetSpent> {
        if std::time::Instant::now() >= self.0 {
            Err(BudgetSpent)
        } else {
            Ok(())
        }
    }
}
/// A request's custom-pattern budget ran out.
#[derive(Debug)]
pub struct BudgetSpent;

/// The char boundary at or before `i`.
fn boundary(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}
/// The char boundary at or after `i`.
fn boundary_up(text: &str, mut i: usize) -> usize {
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[async_trait]
impl Detector for PatternDetector {
    fn kind(&self) -> DetectorKind {
        DetectorKind::Pattern
    }
    fn egress(&self) -> Egress {
        Egress::Local
    }
    fn categories(&self) -> &[Category] {
        &self.categories
    }
    async fn inspect(
        &self,
        _ctx: &DetectContext,
        input: &Inspection<'_>,
    ) -> Result<Findings, DetectorError> {
        self.scan(input).map_err(|_| DetectorError::Timeout)
    }
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
