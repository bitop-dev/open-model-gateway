//! Positives are assembled at test time from a prefix and filler, so no
//! complete credential appears in the source; negatives are ordinary code,
//! identifiers, hashes and encoded data.
use super::*;
use crate::guardrails::detector::Part;

fn rep(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}
fn hex(n: usize) -> String {
    "0123456789abcdef".chars().cycle().take(n).collect()
}
fn alnum(n: usize) -> String {
    "aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0z"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

fn positives() -> Vec<(&'static str, String)> {
    let b64 = |n| alnum(n);
    vec![
        (
            "aws_access_key_id",
            format!("{}{}", "AK", "IA") + &rep('Q', 16),
        ),
        ("github_token", format!("{}_{}", "ghp", alnum(36))),
        (
            "github_fine_grained_token",
            format!("{}_pat_{}", "github", alnum(82)),
        ),
        ("gitlab_token", format!("{}-{}", "glpat", alnum(20))),
        (
            "slack_token",
            format!("{}-{}-{}", "xoxb", hex(12), alnum(24)),
        ),
        (
            "slack_webhook",
            format!(
                "https://hooks.slack.com/services/T{}/B{}/{}",
                rep('A', 9),
                rep('B', 9),
                alnum(24)
            ),
        ),
        ("stripe_secret_key", format!("{}_live_{}", "sk", alnum(24))),
        (
            "openai_api_key",
            format!("{}-proj-{}{}{}", "sk", alnum(24), "T3Blbk", "FJ") + &alnum(24),
        ),
        (
            "anthropic_api_key",
            format!("{}-ant-api03-{}", "sk", alnum(95)),
        ),
        ("google_api_key", format!("{}{}", "AI", "za") + &alnum(35)),
        ("huggingface_token", format!("{}_{}", "hf", alnum(34))),
        ("npm_token", format!("{}_{}", "npm", alnum(36))),
        (
            "private_key",
            format!("-----{} RSA {} KEY-----", "BEGIN", "PRIVATE"),
        ),
        (
            "jwt",
            format!(
                "{}.{}.{}",
                "eyJ".to_owned() + &b64(20),
                "eyJ".to_owned() + &b64(30),
                b64(43)
            ),
        ),
        (
            "connection_string_password",
            format!("{}://app:{}@db.internal:5432/main", "postgres", alnum(16)),
        ),
        (
            "azure_storage_connection_string",
            format!("Account{}={}==", "Key", alnum(86)),
        ),
        (
            "gateway_inference_key",
            format!("{}_{}.{}", "omg", hex(32), hex(64)),
        ),
        (
            "gateway_admin_token",
            format!("{}_admin_{}.{}", "omg", hex(32), hex(64)),
        ),
    ]
}

fn secrets() -> PatternDetector {
    PatternDetector::new(&[Category::Secret], &[]).unwrap()
}
fn found(detector: &PatternDetector, text: &str) -> usize {
    let mut input = Inspection::default();
    input.push(text, Part::Content);
    detector.scan(&input).0.len()
}

#[test]
fn every_builtin_rule_matches_its_shape_inside_prose() {
    let positives = positives();
    assert_eq!(positives.len(), BUILTIN.len(), "one positive per rule");
    let d = secrets();
    for (name, value) in positives {
        let text = format!("please use {value} for the deploy");
        assert!(found(&d, &text) >= 1, "{name} not found");
        // A character glued to either end does not hide it.
        let glued = format!("x{value}x");
        assert!(found(&d, &glued) >= 1, "{name} glued");
    }
}

#[test]
fn ordinary_text_matches_nothing() {
    let d = secrets();
    let negatives = [
        "fn main() { let key = std::env::var(\"API_KEY\")?; println!(\"{key:?}\"); }",
        "550e8400-e29b-41d4-a716-446655440000",
        "sha256: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        "postgres://db.internal:5432/main (no password in the URL)",
        "-----BEGIN PUBLIC KEY-----",
        "The AKIA prefix marks an AWS access key id.",
        "omg_ is the gateway's key prefix; see docs/identity.md",
        "eyJ alone is just the start of a base64 JSON object",
    ];
    for text in negatives {
        assert_eq!(found(&d, text), 0, "{text}");
    }
}

#[test]
fn custom_patterns_match_only_for_their_rule() {
    let custom =
        PatternDetector::new(&[Category::Custom], &["PROJECT-[A-Z]{4}".to_owned()]).unwrap();
    assert_eq!(found(&custom, "status of PROJECT-ABCD and PROJECT-WXYZ"), 2);
    // A custom-only rule never runs the built-in shapes.
    assert_eq!(found(&custom, &format!("{}_{}", "ghp", alnum(36))), 0);
    assert!(PatternDetector::new(&[Category::Custom], &["(unclosed".to_owned()]).is_none());
}

#[test]
fn a_repeated_value_counts_once() {
    let d = secrets();
    let key = format!("{}_{}.{}", "omg", hex(32), hex(64));
    let mut input = Inspection::default();
    let first = format!("key {key}");
    let second = format!("again {key}");
    input.push(&first, Part::Content);
    input.push(&second, Part::ToolArguments);
    let findings = d.scan(&input);
    assert_eq!(findings.0.len(), 2);
    assert_eq!(findings.distinct(&input, Category::Secret), 1);
}

#[test]
fn every_rule_is_bounded_and_linear() {
    // A long run of near-matches is scanned once, not re-tried per position.
    let d = secrets();
    let text = format!("{} ", "AKIA").repeat(200_000);
    let started = std::time::Instant::now();
    assert_eq!(found(&d, &text), 0);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}
