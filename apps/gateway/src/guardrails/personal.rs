//! The personal-data kinds of the pattern detector (#841,
//! docs/guardrails-design.md §2). Each kind is one entry of [`KINDS`]: a
//! candidate pattern (on the `regex` crate: linear time), a validator that
//! keeps only well-formed values (a checksum where the format has one), and
//! the characters that may not touch a match on either side (for the
//! checksummed kinds only a digit, or what would extend the value, so a
//! value glued to a label is still found). Adding a format, such as another
//! country's national identifier, is an entry here, a `Kind` variant, the
//! dashboard's labels and a migration that widens the database's lists.
//!
//! What is deliberately not personal data, and never matches: addresses
//! that are not globally routable (loopback, private, link-local, shared,
//! benchmarking, multicast, broadcast, reserved and documentation ranges),
//! the reserved example domains (`example.com`, `.test`, `.invalid`, …),
//! fictional `555-01xx` numbers and the advertised sample SSNs. Nothing found
//! is kept beyond the request: a finding is a segment index and a span.
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use super::detector::{Budget, Finding, Findings, Inspection};
use super::policy::{Category, Kind};
use super::secrets::MAX_FINDINGS;

/// One personal-data kind.
pub struct KindDef {
    pub kind: Kind,
    /// The redaction label (`[REDACTED:<LABEL>_<n>]`, #841 redact).
    pub label: &'static str,
    /// What it matches, in one plain sentence (docs and the dashboard, which
    /// repeats it word for word: a test compares them).
    pub matches: &'static str,
    pattern: &'static str,
    /// Characters that may not come right before a match.
    before: fn(u8) -> bool,
    /// Characters that may not come right after a match.
    after: fn(u8) -> bool,
    valid: fn(&str) -> bool,
    /// Shorter spans to try, in order, when a candidate fails (a card
    /// followed by an expiry, an IBAN followed by a word): byte ranges of the
    /// segment, around the failed candidate's span.
    shorter: fn(&str, &Range<usize>) -> Vec<Range<usize>>,
    /// Whether a shorter span is checked only after it: its start was cut
    /// out of the candidate (an address's local part trimmed of leading
    /// dots or excess characters), so what came before it is not beside it.
    trimmed_start: bool,
    /// Where the search tries again inside a candidate that is no value, in
    /// order (each later part: a card or IBAN after an earlier token, an
    /// address after a label).
    resume: fn(&str, &Range<usize>) -> Vec<usize>,
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn none(_: &str, _: &Range<usize>) -> Vec<Range<usize>> {
    Vec::new()
}

/// The kinds. Each is scanned on its own, with its own cap: one kind's
/// matches never hide or crowd out another's (a value two kinds match, such
/// as `4111111111111111@pay.io`, counts for both).
pub const KINDS: &[KindDef] = &[
    KindDef {
        kind: Kind::Email,
        label: "EMAIL",
        matches: "Email addresses (local parts in Latin, Greek or Cyrillic letters), except at reserved example domains (example.com, .test, .invalid, .localhost), file names (logo@2x.png) and SSH remotes (git@host:org/repo, …/repo.git).",
        // The local part may hold the letters of scripts written with spaces
        // between words (`andré.dupont@…`, Greek, Cyrillic), not those
        // written without (CJK text glued to an address is not part of
        // it); the domain
        // is ASCII (international names are sent as punycode). Lengths are
        // checked by the validator: counted repetitions of these classes
        // would make the automaton too large to search in linear time
        // without falling back to a slower engine.
        pattern: r"[\p{Latin}\p{Greek}\p{Cyrillic}\p{N}._%+\-]+@(?:[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?\.)+[A-Za-z]+",
        before: |b| alnum(b) || b".%+-_".contains(&b),
        after: |b| alnum(b) || b"-_".contains(&b),
        valid: email,
        shorter: email_trimmed,
        trimmed_start: true,
        resume: after,
    },
    KindDef {
        kind: Kind::Phone,
        label: "PHONE",
        matches: "Phone numbers in international form (+ or 00, then 8 to 15 digits) or North American form with dashes, dots or parentheses ((415) 555-2671, 415-555-2671, 1-415-555-2671); fictional 555-01xx numbers and dates after a + (+2026-11-27) excepted. National formats outside North America are not covered.",
        pattern: r"(?:\+|00)[1-9][0-9 ()\-.]{6,22}[0-9]|(?:1[\-.])?(?:\([2-9][0-9]{2}\) ?|[2-9][0-9]{2}[\-.])[2-9][0-9]{2}[\-.][0-9]{4}",
        before: |b| alnum(b) || b"+-./_".contains(&b),
        after: |b| alnum(b) || b"-/_".contains(&b),
        valid: phone,
        shorter: prefixes_at_separators,
        trimmed_start: false,
        resume: after,
    },
    KindDef {
        kind: Kind::Card,
        label: "CARD",
        matches: "Payment card numbers of the major networks (Visa, Mastercard, Maestro, American Express, Discover, JCB, Diners Club, UnionPay, Mir): 13 to 19 digits, whole or in groups, with a valid check digit (Luhn).",
        pattern: r"[0-9](?:[ \-]?[0-9]){12,18}",
        // Only a digit (which would make it a longer number) disqualifies a
        // card beside it: a letter, an underscore or punctuation does not
        // (`x4111…`, `cc-4111-…`), but digits after `+` are a phone number.
        before: |b| b.is_ascii_digit() || b == b'+',
        after: |b| b.is_ascii_digit(),
        valid: card,
        shorter: group_runs,
        trimmed_start: false,
        resume: later_groups,
    },
    KindDef {
        kind: Kind::Iban,
        label: "IBAN",
        matches: "International bank account numbers (IBAN), in either case, with the length of their country and a valid check (mod 97).",
        pattern: r"[A-Za-z]{2}[0-9]{2}(?: ?[A-Za-z0-9]{4}){2,7}(?: ?[A-Za-z0-9]{1,3})?",
        // Only what would extend it disqualifies an IBAN: an alphanumeric
        // after it. Nothing before it can (it starts with its country
        // code), so `refDE89…` holds one.
        before: |_| false,
        after: alnum,
        valid: iban,
        shorter: iban_prefix,
        trimmed_start: false,
        resume: later_tokens,
    },
    KindDef {
        kind: Kind::UsSsn,
        label: "US_SSN",
        matches: "US Social Security numbers written with dashes (123-45-6789), within the issued ranges. Other countries' identifiers are not built in: use a custom pattern.",
        pattern: r"[0-9]{3}-[0-9]{2}-[0-9]{4}",
        before: |b| word(b) || b"-.".contains(&b),
        after: |b| word(b) || b == b'-',
        valid: ssn,
        shorter: none,
        trimmed_start: false,
        resume: after,
    },
    KindDef {
        kind: Kind::Ip,
        label: "IP",
        matches: "Public IP addresses (IPv4, IPv6 and IPv4 written as IPv6) only: never loopback, private, link-local, shared, multicast, reserved or documentation addresses.",
        pattern: r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}|[0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7}",
        // A version (`v1.2.3.4`) is glued to its letter; a range
        // (`1.2.3.4-5`) to its dash. A label (`ip:8.8.8.8`) or a port
        // (`8.8.8.8:53`) may touch it.
        before: |b| word(b) || b == b'.',
        after: |b| word(b) || b == b'-',
        valid: ip,
        shorter: ip_parts,
        trimmed_start: false,
        resume: past_colon,
    },
];

/// The entry of `kind`.
pub fn def(kind: Kind) -> &'static KindDef {
    KINDS
        .iter()
        .find(|d| d.kind == kind)
        .expect("every kind has an entry")
}

static COMPILED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    KINDS
        .iter()
        .map(|d| Regex::new(d.pattern).expect("personal-data pattern"))
        .collect()
});

/// Whether the match `span` of `text` stands alone: nothing it may not
/// touch right before or after it, and no `.` followed by a digit after it
/// (a longer dotted number, such as a version or an OID).
fn standalone(text: &[u8], span: &Range<usize>, d: &KindDef, check_before: bool) -> bool {
    let before = span.start.checked_sub(1).map(|i| text[i]);
    let after = text.get(span.end).copied();
    if (check_before && before.is_some_and(d.before)) || after.is_some_and(d.after) {
        return false;
    }
    if d.kind == Kind::Email && after == Some(b':') && ssh_remote(text, span) {
        return false;
    }
    if d.kind == Kind::Card && in_hex_digest(text, span) {
        return false;
    }
    // A `00` number right after another digit group is a group of a longer
    // number (an IBAN's `DE89 3704 0044 …`, a table row), not a phone.
    if d.kind == Kind::Phone
        && text[span.clone()].starts_with(b"00")
        && span.start >= 2
        && b" -.".contains(&text[span.start - 1])
        && text[span.start - 2].is_ascii_digit()
    {
        return false;
    }
    !(after == Some(b'.') && text.get(span.end + 1).is_some_and(u8::is_ascii_digit))
}
/// Whether the digits `span` are part of a hex digest: a run of at least
/// 32 hexadecimal characters with a letter in it (an MD5, SHA or commit
/// hash). A letter beside a card does not hide it otherwise (`x4111…`).
/// At most 32 characters are read on each side.
fn in_hex_digest(text: &[u8], span: &Range<usize>) -> bool {
    let hex = |b: &&u8| b.is_ascii_hexdigit();
    let left = text[..span.start]
        .iter()
        .rev()
        .take(32)
        .take_while(hex)
        .count();
    let right = text[span.end..].iter().take(32).take_while(hex).count();
    let run = &text[span.start - left..span.end + right];
    run.len() >= 32 && run.iter().any(u8::is_ascii_alphabetic)
}
/// Whether the address `span` is the user and host of an SSH remote:
/// `git@host:org/repo`, or anything ending `.git` (`user@host:repo.git`);
/// `alice@acme.com:x/y` is an address. The path is what follows the `:` up
/// to the first character a repository path does not hold, at most 512
/// bytes.
fn ssh_remote(text: &[u8], span: &Range<usize>) -> bool {
    let rest = &text[span.end + 1..];
    let rest = &rest[..rest.len().min(512)];
    let path = &rest[..rest
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || b"-_./~".contains(b)))
        .unwrap_or(rest.len())];
    path.ends_with(b".git") || (text[span.clone()].starts_with(b"git@") && path.contains(&b'/'))
}

/// The span of `text` that is a value of `d`: `span` itself, else the
/// first of its shorter spans that is. A shorter span must itself be a whole
/// match of the kind's pattern, so its validator sees the shape it expects.
fn accepted(text: &str, span: Range<usize>, d: &KindDef, re: &Regex) -> Option<Range<usize>> {
    let ok = |r: &Range<usize>, check_before: bool| {
        r.start < r.end
            && r.end <= text.len()
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end)
            && re
                .find(&text[r.clone()])
                .is_some_and(|m| m.range() == (0..r.len()))
            && standalone(text.as_bytes(), r, d, check_before)
            && (d.valid)(&text[r.clone()])
    };
    if ok(&span, true) {
        return Some(span);
    }
    (d.shorter)(text, &span)
        .into_iter()
        .find(|r| ok(r, !(d.trimmed_start && r.start > span.start)))
}

/// Every personal-data finding of every kind in `input`, without a budget
/// (tests and reports). Each kind keeps at most `MAX_FINDINGS` distinct
/// values of its own, so a request full of one kind's values never hides
/// another kind's.
pub fn findings(input: &Inspection<'_>) -> Findings {
    let budget = Budget::steps(u64::MAX);
    Findings(
        Kind::ALL
            .iter()
            .flat_map(|k| findings_of(input, *k, &budget).0.0)
            .collect(),
    )
}

/// The findings of one kind in `input`, and whether the request's budget
/// ran out before the end (checked before each segment and each candidate:
/// the scan's cost is the input's, as a custom pattern's is). A request is scanned only for
/// the kinds its rules name, each once (`DetectContext::personal`).
pub fn findings_of(input: &Inspection<'_>, kind: Kind, budget: &Budget) -> (Findings, bool) {
    let mut found = Vec::new();
    let (d, re) = KINDS
        .iter()
        .zip(COMPILED.iter())
        .find(|(d, _)| d.kind == kind)
        .expect("every kind has an entry");
    // Each kind keeps its first occurrence of at most MAX_FINDINGS distinct
    // values: repeating one value costs nothing and never understates the
    // count of distinct ones.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (index, segment) in input.segments.iter().enumerate() {
        let text: &str = &segment.text;
        if budget.check().is_err() {
            return (Findings(found), true);
        }
        let mut pos = 0;
        while let Some(m) = re.find_at(text, pos) {
            if budget.check().is_err() {
                return (Findings(found), true);
            }
            // A failed candidate may hold a value it cut short (a card after
            // a date or an order number, an IBAN after another token, an
            // address after a label): each of its later parts is tried once,
            // with no further retry, so a candidate costs at most one more
            // search per part (at most eight).
            let span = accepted(text, m.range(), d, re).or_else(|| {
                (d.resume)(text, &m.range())
                    .into_iter()
                    .filter(|p| *p > m.start() && text.is_char_boundary(*p))
                    .find_map(|p| accepted(text, re.find_at(text, p)?.range(), d, re))
            });
            let Some(span) = span else {
                pos = boundary_up(text, m.end().max(m.start() + 1));
                continue;
            };
            pos = span.end.max(m.start() + 1);
            if !seen.insert(&text[span.clone()]) {
                continue;
            }
            if seen.len() > MAX_FINDINGS {
                return (Findings(found), false);
            }
            found.push(Finding {
                category: Category::PersonalData,
                kind: Some(d.kind),
                segment: index,
                span,
            });
        }
    }
    (Findings(found), false)
}

/// The char boundary at or after `i`.
fn boundary_up(text: &str, mut i: usize) -> usize {
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}
/// No retry inside a failed candidate.
fn after(_: &str, _: &Range<usize>) -> Vec<usize> {
    Vec::new()
}
/// Inside a failed card candidate: each later digit group where a card can
/// start (the earlier ones may be a date, a table row or an order number):
/// a group of four (a grouped card's first) or the candidate's last group
/// (cut off by the length cap: a whole card may follow), beginning with a
/// digit some network issues (2 to 6). At most eight.
fn later_groups(text: &str, span: &Range<usize>) -> Vec<usize> {
    let g = groups(text, span);
    let last = g.len().saturating_sub(1);
    g.iter()
        .enumerate()
        .skip(1)
        .filter(|(i, r)| {
            (r.len() == 4 || *i == last) && (b'2'..=b'6').contains(&text.as_bytes()[r.start])
        })
        .take(8)
        .map(|(_, r)| r.start)
        .collect()
}
/// Inside a failed IBAN candidate: each later space-separated token shaped
/// like an IBAN's start, two letters and two digits (the first token may
/// belong to an earlier word: `Ref12 DE89 …`). At most eight.
fn later_tokens(text: &str, span: &Range<usize>) -> Vec<usize> {
    let b = text.as_bytes();
    (span.start..span.end)
        .filter(|&i| b[i] == b' ')
        .map(|i| i + 1)
        .filter(|&p| {
            b.get(p..p + 4).is_some_and(|t| {
                t[..2].iter().all(u8::is_ascii_alphabetic) && t[2..].iter().all(u8::is_ascii_digit)
            })
        })
        .take(8)
        .collect()
}
/// Inside a failed IP candidate: past its first colon (a label glued to an
/// IPv6 address, `ip:2606:4700::1111`).
fn past_colon(text: &str, span: &Range<usize>) -> Vec<usize> {
    text[span.clone()]
        .find(':')
        .map(|i| span.start + i + 1)
        .into_iter()
        .collect()
}

// --- Shorter spans -----------------------------------------------------------------

/// The digit groups of `span` (runs of digits between separators), as
/// byte ranges.
fn groups(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, b) in text.as_bytes()[span.clone()].iter().enumerate() {
        let at = span.start + i;
        match (b.is_ascii_digit(), start) {
            (true, None) => start = Some(at),
            (false, Some(s)) => {
                out.push(s..at);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(s..span.end);
    }
    out
}
/// Runs of whole groups that hold 13 to 19 digits, longest first (a card
/// followed by an expiry or a security code, or preceded by another
/// number). A card has at most five groups, so a candidate of more than
/// eight (a list of short numbers) has none worth trying: the work per
/// candidate stays small whatever the text.
fn group_runs(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    let g = groups(text, span);
    if g.len() > 8 {
        return Vec::new();
    }
    let mut runs = Vec::new();
    for len in (1..g.len()).rev() {
        for i in 0..=g.len() - len {
            let digits: usize = g[i..i + len].iter().map(Range::len).sum();
            if (13..=19).contains(&digits) {
                runs.push(g[i].start..g[i + len - 1].end);
            }
        }
    }
    runs
}
/// An address whose local part is trimmed of its leading dots, and of
/// leading characters beyond the 64 a local part may hold (`.alice@…`, a
/// long run glued before an address).
fn email_trimmed(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    let v = &text[span.clone()];
    let Some(at) = v.rfind('@') else {
        return Vec::new();
    };
    let local = &v[..at];
    let keep = local.char_indices().rev().nth(63).map_or(0, |(i, _)| i);
    let trimmed = local[keep..].trim_start_matches('.');
    let start = span.start + at - trimmed.len();
    if start == span.start || trimmed.is_empty() {
        return Vec::new();
    }
    std::iter::once(start..span.end).collect()
}
/// Prefixes ending at a group's end, longest first (a phone number
/// followed by more digits or punctuation).
fn prefixes_at_separators(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    groups(text, span)
        .iter()
        .rev()
        .skip(1)
        .map(|g| span.start..g.end)
        .collect()
}
/// The prefix with the length its country's IBANs have (an IBAN followed
/// by a word: `BE68 5390 0754 7034 SWIFT`).
fn iban_prefix(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    let v = &text[span.clone()];
    let Some(len) = v
        .get(..2)
        .and_then(|c| IBAN_LENGTHS.iter().find(|(k, _)| k.eq_ignore_ascii_case(c)))
        .map(|(_, l)| *l)
    else {
        return Vec::new();
    };
    let mut seen = 0;
    for (i, c) in v.char_indices() {
        if c != ' ' {
            seen += 1;
            if seen == len {
                return std::iter::once(span.start..span.start + i + 1).collect();
            }
        }
    }
    Vec::new()
}
/// An IPv6 candidate without a trailing `:`, and the IPv4 address an
/// IPv4-mapped one (`::ffff:8.8.8.8`) ends with.
fn ip_parts(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    static DOTTED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:[0-9]{1,3}\.){3}[0-9]{1,3}").expect("dotted quad"));
    let v = &text[span.clone()];
    let mut out = Vec::new();
    if v.ends_with(':') && !v.ends_with("::") {
        out.push(span.start..span.end - 1);
    }
    if let Some(colon) = v.rfind(':')
        && let Some(m) = DOTTED.find(&text[span.start + colon + 1..])
    {
        let at = span.start + colon + 1;
        out.push(at..at + m.end());
    }
    out
}

// --- Validators ------------------------------------------------------------------

/// Domains reserved for examples and testing (RFC 2606, RFC 6761): their
/// addresses belong to no one.
fn reserved_domain(domain: &str) -> bool {
    let d = domain.to_ascii_lowercase();
    ["example.com", "example.net", "example.org"]
        .iter()
        .any(|r| d == *r || d.ends_with(&format!(".{r}")))
        || [".example", ".test", ".invalid", ".localhost"]
            .iter()
            .any(|t| d.ends_with(t))
}
fn email(v: &str) -> bool {
    let Some((local, domain)) = v.rsplit_once('@') else {
        return false;
    };
    let labels: Vec<&str> = domain.split('.').collect();
    let tld = labels.last().copied().unwrap_or("");
    local.chars().count() <= 64
        && labels.len() <= 9
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63)
        && (2..=63).contains(&tld.len())
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && !reserved_domain(domain)
        && !FILE_EXTENSIONS.contains(&tld.to_ascii_lowercase().as_str())
}
/// Final labels that are common file extensions and no top-level domain
/// (`logo@2x.png`): the `@` of a file name, not an address. Extensions
/// that are also domains (`.zip`, `.md`, `.py`, `.rs`, `.sh`) are not here.
const FILE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "avif", "ico", "bmp", "css", "js", "mjs", "cjs",
    "ts", "tsx", "jsx", "json", "html", "htm", "map", "woff", "ttf", "otf", "wasm", "lock", "yaml",
    "yml", "toml", "txt", "pdf", "csv", "log",
];

fn phone(v: &str) -> bool {
    // `00` stands for `+`; written without a separator it is any number
    // that starts with two zeros.
    let international = v.strip_prefix('+').or_else(|| {
        v.strip_prefix("00")
            .filter(|r| r.bytes().any(|b| b" -.()".contains(&b)))
    });
    if let Some(rest) = international {
        let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
        // A date after a `+` (an added line in a diff: `+2026-11-27`,
        // `+20240101`) is no number.
        if digits.len() == 8 && date(&digits) {
            return false;
        }
        // International: 8 to 15 digits, separated by single spaces, dashes
        // or dots, with at most one parenthesised group.
        let opens = rest.matches('(').count();
        if opens > 1 || opens != rest.matches(')').count() {
            return false;
        }
        let separated = rest
            .as_bytes()
            .windows(2)
            .all(|w| !(b" -.".contains(&w[0]) && b" -.".contains(&w[1])));
        return (8..=15).contains(&digits.len()) && separated;
    }
    let digits: String = v.chars().filter(char::is_ascii_digit).collect();
    // North American: NPA-NXX-XXXX (with an optional leading `1` and
    // separator) with one kind of separator, the fictional 555-0100 to
    // 555-0199 excepted.
    if !(10..=11).contains(&digits.len()) {
        return false;
    }
    let national = &digits[digits.len() - 10..];
    let fictional = national[3..6] == *"555" && national[6..].starts_with("01");
    let rest = ["1-", "1."]
        .iter()
        .find_map(|p| v.strip_prefix(p))
        .unwrap_or(v);
    if rest.starts_with('(') {
        return !fictional;
    }
    let seps: Vec<u8> = rest.bytes().filter(|b| !b.is_ascii_digit()).collect();
    seps.windows(2).all(|w| w[0] == w[1]) && !fictional
}

/// Whether eight digits read as a date, YYYYMMDD, from 1900 to 2099.
fn date(d: &str) -> bool {
    let n = |r: Range<usize>| d[r].parse::<u32>().unwrap_or(0);
    (1900..=2099).contains(&n(0..4)) && (1..=12).contains(&n(4..6)) && (1..=31).contains(&n(6..8))
}

fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            let n = u32::from(d - b'0');
            if i % 2 == 1 {
                let x = n * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                n
            }
        })
        .sum();
    sum.is_multiple_of(10)
}
/// Whether `digits` starts with a major network's issuer prefix at a
/// length that network issues. The list covers the major networks, not
/// every regional scheme.
fn issued(digits: &str) -> bool {
    let len = digits.len();
    let p = |n: usize| digits[..n].parse::<u32>().unwrap_or(0);
    (digits.starts_with('4') && [13, 16, 19].contains(&len))
        || (((51..=55).contains(&p(2)) || (2221..=2720).contains(&p(4))) && len == 16)
        || ([34, 37].contains(&p(2)) && len == 15)
        || ((p(4) == 6011 || (644..=649).contains(&p(3)) || p(2) == 65) && (16..=19).contains(&len))
        || ((3528..=3589).contains(&p(4)) && (16..=19).contains(&len))
        || (([36, 38, 39].contains(&p(2)) || (300..=305).contains(&p(3)))
            && (14..=19).contains(&len))
        || (p(2) == 62 && (16..=19).contains(&len))
        // Maestro.
        || ((p(2) == 50 || (56..=58).contains(&p(2)) || [6304, 6759].contains(&p(4))
            || (6761..=6763).contains(&p(4)))
            && (13..=19).contains(&len))
        // Mir.
        || ((2200..=2204).contains(&p(4)) && (16..=19).contains(&len))
}
fn card(v: &str) -> bool {
    let digits: String = v.chars().filter(char::is_ascii_digit).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let seps: Vec<char> = v.chars().filter(|c| !c.is_ascii_digit()).collect();
    if seps.windows(2).any(|w| w[0] != w[1]) {
        return false;
    }
    if !seps.is_empty() {
        // Written in groups: 4-4-4-4(-3), or 4-6-5 / 4-6-4 (Amex, Diners).
        let groups: Vec<usize> = v.split(seps[0]).map(str::len).collect();
        let fours = groups[..groups.len() - 1].iter().all(|g| *g == 4)
            && (1..=4).contains(groups.last().unwrap_or(&0));
        if !(fours || groups == [4, 6, 5] || groups == [4, 6, 4]) {
            return false;
        }
    }
    issued(&digits) && luhn(digits.as_bytes())
}

/// IBAN lengths by country (the SWIFT IBAN registry), as data: a country
/// code and its length, space-separated.
const IBAN_REGISTRY: &str = "AD24 AE23 AL28 AT20 AZ28 BA20 BE16 BG22 BH22 BI27 BR29 BY28 CH21 CR22 \
     CY28 CZ24 DE22 DJ27 DK18 DO28 EE20 EG29 ES24 FI18 FK18 FO18 FR27 GB22 \
     GE22 GI23 GL18 GR27 GT28 HR21 HU28 IE22 IL23 IQ23 IS26 IT27 JO30 KW30 \
     KZ20 LB28 LC32 LI21 LT20 LU20 LV21 LY25 MC27 MD24 ME22 MK19 MN20 MR27 \
     MT31 MU30 NI28 NL18 NO15 OM23 PK24 PL28 PS29 PT25 QA29 RO24 RS22 RU33 \
     SA24 SC31 SD18 SE24 SI19 SK24 SM27 SO23 ST25 SV28 TL23 TN24 TR26 UA29 \
     VA22 VG24 XK20 YE30";
static IBAN_LENGTHS: LazyLock<Vec<(&'static str, usize)>> = LazyLock::new(|| {
    IBAN_REGISTRY
        .split_whitespace()
        .map(|e| (&e[..2], e[2..].parse().expect("an IBAN length")))
        .collect()
});
fn iban(v: &str) -> bool {
    let compact: String = v
        .chars()
        .filter(|c| *c != ' ')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let Some(len) = IBAN_LENGTHS
        .iter()
        .find(|(c, _)| compact.starts_with(c))
        .map(|(_, l)| *l)
    else {
        return false;
    };
    if compact.len() != len {
        return false;
    }
    if v.contains(' ') && v.split(' ').rev().skip(1).any(|g| g.len() != 4) {
        return false;
    }
    // Move the first four characters to the end, letters as 10..35, mod 97.
    let rotated = compact[4..].chars().chain(compact[..4].chars());
    let mut rem = 0u32;
    for c in rotated {
        let n = match c {
            '0'..='9' => u32::from(c as u8 - b'0'),
            'A'..='Z' => u32::from(c as u8 - b'A') + 10,
            _ => return false,
        };
        rem = if n >= 10 {
            (rem * 100 + n) % 97
        } else {
            (rem * 10 + n) % 97
        };
    }
    rem == 1
}

fn ssn(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() != 11 || b[3] != b[6] {
        return false;
    }
    let area: u32 = v[..3].parse().unwrap_or(0);
    let group = &v[4..6];
    let serial = &v[7..];
    // The advertised samples (a wallet insert, an advertisement) were voided.
    area != 0
        && area != 666
        && area < 900
        && group != "00"
        && serial != "0000"
        && !["078-05-1120", "219-09-9999"].contains(&v.replace(' ', "-").as_str())
}

/// Whether an IPv4 address is globally routable (IANA special-purpose
/// registry): none of the non-global ranges below.
pub fn global_v4(a: Ipv4Addr) -> bool {
    let [o1, o2, o3, _] = a.octets();
    !(o1 == 0
        || o1 == 10
        || (o1 == 100 && (64..=127).contains(&o2))
        || o1 == 127
        || (o1 == 169 && o2 == 254)
        || (o1 == 172 && (16..=31).contains(&o2))
        || (o1 == 192 && o2 == 0 && (o3 == 0 || o3 == 2))
        || (o1 == 192 && o2 == 88 && o3 == 99)
        || (o1 == 192 && o2 == 168)
        || (o1 == 198 && (18..=19).contains(&o2))
        || (o1 == 198 && o2 == 51 && o3 == 100)
        || (o1 == 203 && o2 == 0 && o3 == 113)
        || o1 >= 224)
}
/// Whether an IPv6 address is globally routable: global unicast (2000::/3)
/// outside the documentation (2001:db8::/32, 3fff::/20), IETF protocol
/// (2001::/23) and 6to4 (2002::/16) ranges.
pub fn global_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        && !(s[0] == 0x2001 && s[1] < 0x0200)
        && s[0] != 0x2002
        // 3fff::/20, documentation since 2024 (RFC 9637).
        && !(s[0] == 0x3fff && s[1] < 0x1000)
}
fn ip(v: &str) -> bool {
    if v.contains('.') {
        // No leading zeros: `010.1.1.1` is not written by anyone meaning an address.
        if v.split('.').any(|o| o.len() > 1 && o.starts_with('0')) {
            return false;
        }
        return v.parse::<Ipv4Addr>().is_ok_and(global_v4);
    }
    v.parse::<Ipv6Addr>().is_ok_and(global_v6)
}

#[cfg(test)]
#[path = "personal_tests.rs"]
mod tests;
