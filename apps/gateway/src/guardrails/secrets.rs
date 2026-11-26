//! The pattern detector (docs/guardrails-design.md §4): the built-in secret
//! rules and a rule's custom patterns, on the `regex` crate (linear time, no
//! backtracking). Nothing leaves the gateway.
//!
//! The built-in rules match credentials by their documented shape: a fixed
//! prefix or structure, so ordinary code, UUIDs, hashes and base64 data do not
//! match. A match is data, not a credential, when it sits inside an encoded
//! blob: glued to a run of eight or more base64url characters (letters,
//! digits, `_`, `-`) on either side. A run of at most 32 characters that is a
//! label (snake or kebab case words, the separator against the token, as in
//! `deploy_token-` or `aws-access-key-`) is not glue. Quotes, spaces, `=`,
//! `:` and escapes such as `\n` or `%20` around a credential do not hide it.
//! A PEM or PGP private key is matched on its `BEGIN` line, which is enough to
//! refuse it and keeps the match short.
use std::sync::LazyLock;

use regex::{Regex, RegexSet};

use super::detector::{
    Budget, BudgetSpent, DetectContext, Detector, DetectorError, Finding, Findings, Inspected,
    Inspection,
};
use super::policy::{Category, DetectorKind, Egress};

/// The built-in secret shapes, by name (names are for tests and docs only;
/// a decision records the rule and the category `secret`).
pub const BUILTIN: &[(&str, &str)] = &[
    ("aws_access_key_id", r"(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}"),
    (
        "aws_secret_access_key",
        // The identifier the keyword ends (`prodAwsSecretAccessKey`, AWS's
        // own JSON `SecretAccessKey`) is part of the match, so it never glues
        // the match to data.
        r#"(?i:[a-z0-9_]*secret_?access_?key)["']?\s*[:=]\s*["']?[A-Za-z0-9/+]{40}"#,
    ),
    (
        "github_token",
        r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}",
    ),
    ("github_fine_grained_token", r"github_pat_[A-Za-z0-9_]{82}"),
    ("gitlab_token", r"glpat-[A-Za-z0-9_\-]{20,}"),
    ("slack_token", r"(?:xox[abposr]|xapp)-[A-Za-z0-9-]{10,250}"),
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
        r"sk-ant-(?:api|admin|oat)[0-9]{2}-[A-Za-z0-9_\-]{80,120}",
    ),
    ("openrouter_api_key", r"sk-or-v1-[0-9a-f]{64}"),
    ("google_api_key", r"AIza[0-9A-Za-z_\-]{35}"),
    ("huggingface_token", r"hf_[A-Za-z0-9]{34,40}"),
    ("npm_token", r"npm_[A-Za-z0-9]{36}"),
    (
        "private_key",
        r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
    ),
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{16,}",
    ),
    (
        "connection_string_password",
        r"(?i:postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|rediss?|amqps?|mssql|sqlserver)(?i:\+[a-z0-9]{1,32})?://[^\s:/@]{0,256}:[^\s@/]{1,256}@[^\s/]{1,256}",
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
/// an encoded blob, unless the run is a label of at most `LABEL_MAX` bytes.
const BLOB_RUN: usize = 8;
const LABEL_MAX: usize = 32;
/// A word of a label: letters of one case with digits anywhere (`token`,
/// `KEY`, `k8s`, `OAUTH2TOKEN`, `v2`), PascalCase words then digits
/// (`DeployKey`, `Prod1`), digits alone, or empty between two separators.
/// camelCase (`forUnitTests`, common in code) and random mixes (`aB3dE5`)
/// are not label words.
fn word(w: &[u8]) -> bool {
    let one_case = |f: fn(&u8) -> bool| w.iter().all(|b| f(b) || b.is_ascii_digit());
    let pascal = || {
        let letters = w.iter().take_while(|b| b.is_ascii_alphabetic()).count();
        let (alpha, digits) = w.split_at(letters);
        alpha.first().is_some_and(u8::is_ascii_uppercase)
            && alpha
                .windows(2)
                .all(|p| !(p[0].is_ascii_uppercase() && p[1].is_ascii_uppercase()))
            && digits.iter().all(u8::is_ascii_digit)
    };
    one_case(u8::is_ascii_lowercase) || one_case(u8::is_ascii_uppercase) || pascal()
}
/// Snake or kebab case words with the separator against the token.
fn label(run: &[u8], before: bool) -> bool {
    let sep = |b: &u8| *b == b'_' || *b == b'-';
    let (edge, words) = if before {
        (run.last(), &run[..run.len().saturating_sub(1)])
    } else {
        (run.first(), run.get(1..).unwrap_or_default())
    };
    edge.is_some_and(sep) && words.split(sep).all(word)
}
/// Whether a run of base64url characters next to a match glues it into data.
fn glued(run: &[u8], before: bool) -> bool {
    run.len() >= BLOB_RUN && !(run.len() <= LABEL_MAX && label(run, before))
}
/// Shapes whose match ends in its own delimiter (a PEM header's trailing
/// `-----`): what follows is the key's body, never glue.
const DELIMITED_AFTER: &[&str] = &["private_key"];

/// Whether `span` of `text` stands apart from base64url data around it (only
/// before it when `delimited` says the match ends in its own delimiter).
fn standalone(text: &[u8], span: std::ops::Range<usize>, delimited: bool) -> bool {
    let start = span.start
        - text[..span.start]
            .iter()
            .rev()
            .take(LABEL_MAX + 1)
            .take_while(|b| encoded(**b))
            .count();
    let end = span.end
        + text[span.end..]
            .iter()
            .take(LABEL_MAX + 1)
            .take_while(|b| encoded(**b))
            .count();
    !glued(&text[start..span.start], true) && (delimited || !glued(&text[span.end..end], false))
}

/// Every match of the built-in secret shapes (shared by every rule that has
/// the `secret` category in one request).
pub fn builtin(input: &Inspection<'_>) -> Findings {
    let mut found = Vec::new();
    for (index, segment) in input.segments.iter().enumerate() {
        for i in SECRETS.any.matches(&segment.text).iter() {
            for m in SECRETS.each[i].find_iter(&segment.text).filter(|m| {
                standalone(
                    segment.text.as_bytes(),
                    m.range(),
                    DELIMITED_AFTER.contains(&BUILTIN[i].0),
                )
            }) {
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
/// of `patterns::compile_within`) for `custom`.
pub struct PatternDetector {
    categories: Vec<Category>,
    secrets: bool,
    custom: Vec<Regex>,
}

/// A rule's custom-pattern matches, and whether its scan ran out of budget
/// before the end (the matches found until then are kept).
pub struct Custom {
    pub findings: Findings,
    pub spent: bool,
}

/// How far ahead one custom-pattern call looks; after a window without a
/// match the next starts `OVERLAP` bytes before its end; a match that ends
/// within `OVERLAP` of the window's end is searched again up to `EXTEND`
/// bytes from its start.
const WINDOW: usize = 4096;
const OVERLAP: usize = 1024;
const EXTEND: usize = 8192;

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

impl PatternDetector {
    /// `None` when a custom pattern no longer compiles within a local
    /// layer's limits (the rule then fails like a detector that cannot run).
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
    /// Whether the rule has custom patterns.
    pub fn has_custom(&self) -> bool {
        !self.custom.is_empty()
    }
    /// Matches of the rule's custom patterns, within a budget: a custom
    /// pattern may be any user's, so its cost per byte is too. Each step
    /// searches the text from its position, with the text before it as
    /// context (so `\b` and `^` mean what they mean in the whole text), but
    /// only up to `WINDOW` bytes ahead, so one regex call is bounded; the
    /// budget is checked before every call. A match that ends exactly at that
    /// horizon is searched again with `EXTEND` bytes more: the match found
    /// from the same position (the same one, longer, or a later one the cut
    /// had hidden) is the one counted, and the scan resumes after it. A value
    /// longer than `OVERLAP` that its pattern matches only whole (say
    /// `BEGIN…END`) can be missed when it starts more than `OVERLAP` bytes
    /// before a horizon; empty matches never count. What was found before
    /// the budget ran out is kept.
    pub fn custom_findings(&self, input: &Inspection<'_>, budget: &Budget) -> Custom {
        let mut found = Vec::new();
        let spent = self.scan_custom(input, budget, &mut found).is_err();
        Custom {
            findings: Findings(found),
            spent,
        }
    }
    fn scan_custom(
        &self,
        input: &Inspection<'_>,
        budget: &Budget,
        found: &mut Vec<Finding>,
    ) -> Result<(), BudgetSpent> {
        for (index, segment) in input.segments.iter().enumerate() {
            let text: &str = &segment.text;
            for pattern in &self.custom {
                let mut pos = 0;
                while pos < text.len() {
                    budget.check()?;
                    let end = boundary(text, (pos + WINDOW).min(text.len()));
                    let Some(cut) = pattern.find_at(&text[..end], pos) else {
                        if end == text.len() {
                            break;
                        }
                        let back = boundary(text, end - OVERLAP.min(end - pos));
                        pos = boundary_up(text, back.max(pos + 1));
                        continue;
                    };
                    // A match within `OVERLAP` of a cut window may be a
                    // shorter alternative of one that runs past the cut (or
                    // exist only because of the cut): search the same start
                    // again with the text going on. A first match nearer the
                    // start needs no second call, so dense matches cost one
                    // call each.
                    let found_here = if end < text.len() && cut.end() + OVERLAP > end {
                        budget.check()?;
                        let further = boundary(text, (cut.start() + EXTEND).min(text.len()));
                        pattern.find_at(&text[..further], pos)
                    } else {
                        Some(cut)
                    };
                    let Some(m) = found_here else {
                        // Nothing starts at or after `pos` once the text goes
                        // on: the cut made that match.
                        pos = boundary_up(text, cut.start() + 1);
                        continue;
                    };
                    if !m.is_empty() {
                        if found.len() == MAX_FINDINGS {
                            return Ok(());
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
        Ok(())
    }
}

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
    /// Distinct values per category: the shared built-in findings for
    /// `secret`, the rule's custom patterns for `custom` (incomplete when the
    /// budget ran out first).
    fn inspect(
        &self,
        ctx: &DetectContext<'_>,
        input: &Inspection<'_>,
    ) -> Result<Inspected, DetectorError> {
        let custom = self
            .has_custom()
            .then(|| self.custom_findings(input, ctx.budget));
        let counts = self
            .categories
            .iter()
            .map(|&category| {
                let n = if category == Category::Secret && self.secrets {
                    ctx.builtin
                        .get_or_init(|| builtin(input))
                        .distinct(input, category)
                } else {
                    custom
                        .as_ref()
                        .map_or(0, |c| c.findings.distinct(input, category))
                };
                (category, n)
            })
            .collect();
        Ok(Inspected {
            counts,
            complete: custom.is_none_or(|c| !c.spent),
        })
    }
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
