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
        let custom = patterns
            .iter()
            .map(|p| super::patterns::compile(p))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            categories: categories.to_vec(),
            secrets: categories.contains(&Category::Secret),
            custom,
        })
    }
    /// Every match, synchronously (the detector never waits on anything).
    pub fn scan(&self, input: &Inspection<'_>) -> Findings {
        let mut found = if self.secrets {
            builtin(input).0
        } else {
            Vec::new()
        };
        found.extend(self.custom_findings(input).0);
        Findings(found)
    }
    /// Whether the built-in secret shapes run for this rule.
    pub fn secrets(&self) -> bool {
        self.secrets
    }
    /// Matches of the rule's custom patterns, in time linear in the text
    /// (a custom pattern is an administrator's, so its cost is bounded
    /// whatever it is): each step finds the earliest end of a match, which
    /// scans no further than that end, then the match's start within the text
    /// just scanned. Patterns that can match the empty string are refused at
    /// save; an empty match here never counts.
    pub fn custom_findings(&self, input: &Inspection<'_>) -> Findings {
        let mut found = Vec::new();
        for (index, segment) in input.segments.iter().enumerate() {
            let text: &str = &segment.text;
            for pattern in &self.custom {
                let mut at = 0;
                while at < text.len() {
                    let Some(end) = pattern.shortest_match_at(text, at) else {
                        break;
                    };
                    let span = pattern
                        .find_at(&text[..end], at)
                        .map_or(at..end, |m| m.range());
                    if found.len() == MAX_FINDINGS {
                        return Findings(found);
                    }
                    if !span.is_empty() {
                        found.push(Finding {
                            category: Category::Custom,
                            segment: index,
                            span,
                        });
                    }
                    at = if end > at { end } else { next_char(text, at) };
                }
            }
        }
        Findings(found)
    }
}

/// The byte offset after the character at `at`.
fn next_char(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .next()
        .map_or(text.len(), |c| at + c.len_utf8())
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
        Ok(self.scan(input))
    }
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
