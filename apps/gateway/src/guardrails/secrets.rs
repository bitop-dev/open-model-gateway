//! The pattern detector (docs/guardrails-design.md §4): the built-in secret
//! rules and a rule's custom patterns, on the `regex` crate (linear time, no
//! backtracking). Nothing leaves the gateway.
//!
//! The built-in rules match credentials by their documented shape: a fixed
//! prefix or structure, so ordinary code, UUIDs, hashes and base64 data do not
//! match. A PEM private key is matched on its `BEGIN` line, which is enough to
//! refuse it and keeps the match short.
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::{Regex, RegexSet};

use super::detector::{DetectContext, Detector, DetectorError, Finding, Findings, Inspection};
use super::policy::{Category, DetectorKind, Egress};

/// The built-in secret shapes, by name (names are for tests and docs only;
/// a decision records the rule and the category `secret`).
pub const BUILTIN: &[(&str, &str)] = &[
    (
        "aws_access_key_id",
        r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
    ),
    (
        "github_token",
        r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}\b",
    ),
    (
        "github_fine_grained_token",
        r"\bgithub_pat_[A-Za-z0-9_]{82}\b",
    ),
    ("gitlab_token", r"\bglpat-[A-Za-z0-9_\-]{20,}\b"),
    ("slack_token", r"\bxox[abposr]-[A-Za-z0-9-]{10,250}\b"),
    (
        "slack_webhook",
        r"https://hooks\.slack\.com/services/T[A-Z0-9]{8,}/B[A-Z0-9]{8,}/[A-Za-z0-9]{24}",
    ),
    (
        "stripe_secret_key",
        r"\b(?:sk|rk)_live_[A-Za-z0-9]{24,99}\b",
    ),
    (
        "openai_api_key",
        r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}T3BlbkFJ[A-Za-z0-9_\-]{20,}",
    ),
    (
        "anthropic_api_key",
        r"\bsk-ant-(?:api|admin)[0-9]{2}-[A-Za-z0-9_\-]{80,120}",
    ),
    ("google_api_key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
    ("huggingface_token", r"\bhf_[A-Za-z0-9]{34,40}\b"),
    ("npm_token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    ("private_key", r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{16,}",
    ),
    (
        "connection_string_password",
        r"\b(?:postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|rediss?|amqps?|mssql|sqlserver)://[^\s:/@]{1,256}:[^\s@/]{1,256}@[^\s/]{1,256}",
    ),
    (
        "azure_storage_connection_string",
        r"AccountKey=[A-Za-z0-9+/]{80,}={0,2}",
    ),
    (
        "gateway_inference_key",
        r"\bomg_[0-9a-fA-F]{32}\.[0-9a-fA-F]{64}\b",
    ),
    (
        "gateway_admin_token",
        r"\bomg_admin_[0-9a-fA-F]{32}\.[0-9a-fA-F]{64}\b",
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
        let mut found = Vec::new();
        for (index, segment) in input.segments.iter().enumerate() {
            if self.secrets {
                for i in SECRETS.any.matches(segment.text).iter() {
                    for m in SECRETS.each[i].find_iter(segment.text) {
                        found.push(Finding {
                            category: Category::Secret,
                            segment: index,
                            span: m.range(),
                        });
                    }
                }
            }
            for pattern in &self.custom {
                for m in pattern.find_iter(segment.text) {
                    found.push(Finding {
                        category: Category::Custom,
                        segment: index,
                        span: m.range(),
                    });
                }
            }
        }
        Findings(found)
    }
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
