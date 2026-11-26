//! Positives are assembled at test time from a prefix and filler, so no
//! complete credential appears in the source; negatives are ordinary code,
//! identifiers, hashes and encoded data. Budgets are step counts, so no
//! assertion depends on the machine's speed.
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
/// A deterministic stream of bytes (xorshift).
fn noise(seed: u64) -> impl Iterator<Item = u64> {
    let mut x = seed;
    std::iter::repeat_with(move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    })
}

fn positives() -> Vec<(&'static str, String)> {
    vec![
        (
            "aws_access_key_id",
            format!("{}{}", "AK", "IA") + &rep('Q', 16),
        ),
        (
            "aws_secret_access_key",
            format!("aws_secret_{}_key = \"{}\"", "access", alnum(40)),
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
                "eyJ".to_owned() + &alnum(20),
                "eyJ".to_owned() + &alnum(30),
                alnum(43)
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

fn input(text: &str) -> Inspection<'_> {
    let mut input = Inspection::default();
    input.push(text, Part::Content);
    input
}
fn found(text: &str) -> usize {
    builtin(&input(text)).0.len()
}
fn custom(pattern: &str) -> PatternDetector {
    PatternDetector::new(&[Category::Custom], &[pattern.to_owned()]).unwrap()
}
fn spans(d: &PatternDetector, text: &str) -> Vec<String> {
    let input = input(text);
    let out = d.custom_findings(&input, &Budget::steps(1_000_000));
    assert!(!out.spent);
    out.findings
        .0
        .iter()
        .map(|f| text[f.span.clone()].to_owned())
        .collect()
}

#[test]
fn every_builtin_rule_matches_its_shape_in_prose_punctuation_and_escapes() {
    let positives = positives();
    assert_eq!(positives.len(), BUILTIN.len(), "one positive per rule");
    for (name, value) in positives {
        for around in [
            format!("please use {value} for the deploy"),
            format!("\"{value}\""),
            format!("key={value};"),
            format!("({value})"),
            format!("\\n{value}"),
            format!("%20{value}%22"),
            format!("x{value}x"),
        ] {
            assert!(found(&around) >= 1, "{name} in {around}");
        }
        // A labelled AWS secret key carries a keyword, `=` and quotes that
        // encoded data never has, and absorbs the identifier before it.
        if name == "aws_secret_access_key" {
            continue;
        }
        assert_eq!(
            found(&format!("Zx9aB3dE5{value}_Q")),
            0,
            "{name} inside base64url"
        );
    }
    // The newer shapes of the review: PGP blocks, a driver suffix, no user.
    for value in [
        format!("-----{} PGP {} KEY BLOCK-----", "BEGIN", "PRIVATE"),
        format!("{}+psycopg2://app:{}@db:5432/x", "postgresql", alnum(12)),
        format!("{}://:{}@cache:6379", "redis", alnum(12)),
        format!("{}-ant-oat01-{}", "sk", alnum(95)),
    ] {
        assert!(found(&value) >= 1, "{value}");
    }
}

#[test]
fn a_label_before_or_after_a_token_does_not_hide_it() {
    let tokens = [
        format!("{}_{}", "ghp", alnum(36)),
        format!("{}{}", "AK", "IA") + &rep('Q', 16),
        format!("{}_{}.{}", "omg", hex(32), hex(64)),
    ];
    let before = [
        "key_",
        "deploy_token-",
        "access_token_",
        "aws-access-key-",
        "PROD_DEPLOY_TOKEN_",
        "my-service-account-api-token-v2-",
        "api_key_1_",
        "__secret_",
        "DeployKey_",
        "ProdDeployToken-",
        "K8S_DEPLOY_",
        "OAUTH2TOKEN_",
        "s3bucket-",
    ];
    for token in &tokens {
        for label in before {
            assert!(label.len() <= 32);
            assert!(found(&format!("{label}{token}")) >= 1, "{label}{token}");
        }
        for label in [
            "_prod",
            "-staging-v2",
            "_PRIMARY_KEY",
            "_Prod_Key",
            "-1",
            "__",
            "_k8s_prod",
        ] {
            assert!(found(&format!("{token}{label}")) >= 1, "{token}{label}");
        }
    }
    // Longer than a label, or not a label's shape: glue.
    let glued = format!("{}{}", "abcdefgh".repeat(5), tokens[0]);
    assert_eq!(found(&glued), 0, "a 40-byte run is data");
    assert_eq!(
        found(&format!("aB3dE5gH7j_{}", tokens[0])),
        0,
        "letters and digits mixed is data"
    );
    // camelCase is code, not a label: a loose token shape inside an
    // identifier stays data.
    let identifier = format!(
        "ta{}_live_streamingResponseHandlerFactory_forUnitTests",
        "sk"
    );
    assert_eq!(found(&identifier), 0, "{identifier}");
    // A labelled AWS secret key behind any identifier.
    let value = alnum(40);
    for (case, line) in [
        format!("const productionAwsSecretAccessKey = \"{value}\""),
        format!("{}_aws_secret_access_key: {value}", "x".repeat(60)),
        format!("AWS_SECRET_ACCESS_KEY={value}"),
    ]
    .iter()
    .enumerate()
    {
        assert!(found(line) >= 1, "labelled AWS key, case {case}");
    }
}

#[test]
fn ordinary_and_encoded_data_matches_nothing() {
    let mut corpus = Vec::new();
    // 1 MiB each of random base64 and base64url.
    for (seed, alphabet) in [
        (
            7_u64,
            &b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"[..],
        ),
        (
            11,
            &b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"[..],
        ),
    ] {
        corpus.push(
            noise(seed)
                .take(1 << 20)
                .map(|x| alphabet[(x % 64) as usize] as char)
                .collect::<String>(),
        );
    }
    let mut ids = noise(13);
    for _ in 0..2_000 {
        let a = ids.next().unwrap();
        let b = ids.next().unwrap();
        corpus.push(uuid::Uuid::from_u64_pair(a, b).to_string());
        corpus.push(format!("{a:016x}{b:016x}{:08x}", a as u32)); // a git SHA
        corpus.push(format!("{a:016x}{b:016x}{a:016x}{b:016x}")); // a SHA-256
    }
    corpus.extend(
        [
            "fn main() { let key = std::env::var(\"API_KEY\")?; println!(\"{key:?}\"); }",
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
            "postgres://db.internal:5432/main (no password in the URL)",
            "-----BEGIN PUBLIC KEY-----",
            "The AKIA prefix marks an AWS access key id.",
            "omg_ is the gateway's key prefix; see docs/identity.md",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0",
            "eyJ.eyJ.x and eyJ alone",
            "my-component-name-v2 service-account-token-rotation ghp-style-name hf_hub",
        ]
        .map(str::to_owned),
    );
    for text in &corpus {
        assert_eq!(found(text), 0, "{}", &text[..text.len().min(80)]);
    }
}

#[test]
fn custom_patterns_match_only_for_their_rule() {
    let d = custom("PROJECT-[A-Z]{4}");
    assert_eq!(
        spans(&d, "status of PROJECT-ABCD and PROJECT-WXYZ").len(),
        2
    );
    // A custom-only rule never runs the built-in shapes.
    assert!(spans(&d, &format!("{}_{}", "ghp", alnum(36))).is_empty());
    assert!(PatternDetector::new(&[Category::Custom], &["(unclosed".to_owned()]).is_none());
}

#[test]
fn a_repeated_value_counts_once() {
    let key = format!("{}_{}.{}", "omg", hex(32), hex(64));
    let mut input = Inspection::default();
    let first = format!("key {key}");
    let second = format!("again {key}");
    input.push(&first, Part::Content);
    input.push(&second, Part::ToolArguments);
    let findings = builtin(&input);
    assert_eq!(findings.0.len(), 2);
    assert_eq!(findings.distinct(&input, Category::Secret), 1);
}

#[test]
fn a_match_hidden_by_a_window_edge_is_still_found_and_counted_once() {
    // The first window ends right after `abcdef`: `$` matched only because
    // of the cut; the `c` inside it is the real match.
    let text = format!("{}abcdefzzz", "x".repeat(4090));
    assert_eq!(spans(&custom("abcdef$|c"), &text), ["c"]);
    // `\b` matched at the cut; `bcd` starts inside the cut match.
    let text = format!("{}abcdef rest", "x".repeat(4093));
    assert_eq!(spans(&custom(r"abc\b|bcd"), &text), ["bcd"]);
    // A match that starts before a `\b` the cut made is found whole.
    let text = format!("{}ID-1234567890x", ".".repeat(4089));
    assert_eq!(
        spans(&custom(r"ID-[0-9]{10}|[0-9]{4}\b"), &text),
        ["ID-1234567890"]
    );
    // A shorter alternative that ends before the cut is not taken for the
    // longer one that runs past it, nor for one that starts earlier.
    let text = format!("{}xabcy tail", "q".repeat(4093));
    assert_eq!(spans(&custom("x.{0,10}y|x"), &text), ["xabcy"]);
    let text = format!("{}xacby tail", "q".repeat(4092));
    assert_eq!(spans(&custom("x.{0,10}y|c"), &text), ["xacby"]);
    // Dense matches near a window end each cost one call.
    let d = custom("[0-9]");
    let text = format!("{}{}", "q".repeat(3500), "1".repeat(3000));
    let input_ = input(&text);
    let budget = Budget::steps(1_000_000);
    let out = d.custom_findings(&input_, &budget);
    assert_eq!(out.findings.0.len(), 3000);
    assert!(1_000_000 - budget.left().unwrap() <= 3000 + 4);
    // A longer match re-found past the cut is counted once, whole.
    let text = format!("{}1234567890 tail", "x".repeat(4090));
    assert_eq!(spans(&custom("[0-9]+"), &text), ["1234567890"]);
}

#[test]
fn windows_neither_lose_long_values_nor_invent_boundaries() {
    let text = format!("{}password=hunter2{}", "x ".repeat(1498), "A".repeat(1200));
    let found = spans(&custom(r"(?i)password\s*[:=]\s*\S+"), &text);
    assert_eq!(found, [&text[2996..]]);
    let text = format!("{}yfoo{}", "y".repeat(3071), " ".repeat(2000));
    assert!(
        spans(&custom(r"\bfoo\b"), &text).is_empty(),
        "no boundary at a window start"
    );
    let text = format!("{}{} tail", " ".repeat(4080), "1".repeat(20));
    assert!(
        spans(&custom(r"\b[0-9]{16}\b"), &text).is_empty(),
        "no boundary at a cut"
    );
    let text = format!("{}ACME-123456 and ACME-123456", "x".repeat(4090));
    let d = custom("ACME-[0-9]+");
    let input = input(&text);
    let out = d.custom_findings(&input, &Budget::steps(1_000));
    assert_eq!(out.findings.distinct(&input, Category::Custom), 1);
}

#[test]
fn the_budget_bounds_every_scan_in_regex_calls() {
    // `find_iter` is quadratic for this pattern on this text; each window
    // call here is bounded, and the scan stops at the match cap.
    let hostile = custom("[a-z]+X|a");
    let text = "a".repeat(2 << 20);
    let input_ = input(&text);
    let budget = Budget::steps(1_000_000);
    let out = hostile.custom_findings(&input_, &budget);
    assert_eq!(out.findings.0.len(), MAX_FINDINGS);
    assert!(1_000_000 - budget.left().unwrap() <= MAX_FINDINGS as u64 + 1);
    // A spent budget stops the scan and keeps what was found.
    let budget = Budget::steps(3);
    let out = hostile.custom_findings(&input_, &budget);
    assert!(out.spent);
    assert_eq!(
        out.findings.0.len(),
        3,
        "one match per step until the budget ran out, all kept"
    );
    // A short segment that never matches still takes a step.
    let out = custom("qqqq").custom_findings(&input("short"), &Budget::steps(0));
    assert!(out.spent);
}

#[test]
fn a_scan_keeps_a_bounded_number_of_matches() {
    let d = custom("x");
    let text = "x".repeat(MAX_FINDINGS * 3);
    let out = d.custom_findings(&input(&text), &Budget::steps(u64::MAX));
    assert_eq!(out.findings.0.len(), MAX_FINDINGS);
    assert!(d.has_custom());
}
