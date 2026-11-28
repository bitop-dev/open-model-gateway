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
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use super::detector::{Budget, Finding, Findings, Inspection};
use super::policy::{Category, Kind};
use super::secrets::MAX_FINDINGS;

mod valid;
use valid::{IBAN_LENGTHS, card, email, iban, ip, phone, ssn};

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
    /// order (each later part: a card or IBAN after an earlier token, an IP
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
        matches: "Email addresses (local parts in Latin, Greek or Cyrillic letters), except at reserved example domains (example.com, .test, .invalid, .localhost), file names (logo@2x.png) and SSH remotes (git@host:org/repo, host:repo.git).",
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
        // (`x4111…`, `cc-4111-…`). After a `+` it is a phone number only
        // when it reads as one (`standalone`).
        before: |b| b.is_ascii_digit(),
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

/// Whether the match `span` of `text` stands alone (what comes before it is
/// checked only when `check_before`: a span whose start was trimmed out of
/// a candidate has none beside it): nothing it may not
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
    if d.kind == Kind::Iban && in_hex_digest(text, span) {
        return false;
    }
    if d.kind == Kind::Card
        && (in_hex_digest(text, span)
            || (before == Some(b'+')
                && std::str::from_utf8(&text[span.start - 1..span.end]).is_ok_and(phone)))
    {
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
/// Whether the value `span` (a card's digits, or an IBAN written with
/// only hexadecimal characters) is part of a hex digest: a run of at least
/// 32 hexadecimal characters with a letter beside the value (an MD5, SHA
/// or commit hash). A letter beside a card or IBAN does not hide it otherwise
/// (`x4111…`). At most 32 characters are read on each side.
fn in_hex_digest(text: &[u8], span: &Range<usize>) -> bool {
    // A digest has no separators: a grouped value is never inside one.
    if !text[span.clone()].iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    let hex = |b: &&u8| b.is_ascii_hexdigit();
    let left = text[..span.start]
        .iter()
        .rev()
        .take(32)
        .take_while(hex)
        .count();
    let right = text[span.end..].iter().take(32).take_while(hex).count();
    // The letter is beside the value, not its own (an IBAN's country code
    // after other digits is no digest).
    let beside = text[span.start - left..span.start]
        .iter()
        .chain(&text[span.end..span.end + right])
        .any(u8::is_ascii_alphabetic);
    left + span.len() + right >= 32 && beside
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
    // `span` is a match of the pattern; a shorter span is checked to be one
    // too before a validator sees it.
    let ok = |r: &Range<usize>, check_before: bool, matched: bool| {
        r.start < r.end
            && r.end <= text.len()
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end)
            && (matched
                || re
                    .find(&text[r.clone()])
                    .is_some_and(|m| m.range() == (0..r.len())))
            && standalone(text.as_bytes(), r, d, check_before)
            && (d.valid)(&text[r.clone()])
    };
    if ok(&span, true, true) {
        return Some(span);
    }
    (d.shorter)(text, &span)
        .into_iter()
        .find(|r| ok(r, !(d.trimmed_start && r.start > span.start), false))
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
        // Starts of candidates that failed, at or after `pos`: a candidate
        // is the leftmost-first match at its start, so one that failed once
        // fails again, and overlapping candidates retry each start once.
        let mut failed = std::collections::BTreeSet::new();
        while let Some(m) = re.find_at(text, pos) {
            if budget.check().is_err() {
                return (Findings(found), true);
            }
            if failed.len() > 64 {
                failed = failed.split_off(&pos);
            }
            // A failed candidate may hold a value it cut short (a card after
            // a date or an order number, an IBAN after another token, an
            // IP address after a label): each of its later parts is tried
            // once, with no further retry, so a candidate costs at most one
            // more search per part (at most eight).
            let mut span = None;
            if !failed.contains(&m.start()) {
                span = accepted(text, m.range(), d, re);
                if span.is_none() {
                    failed.insert(m.start());
                    for p in (d.resume)(text, &m.range()) {
                        if p <= m.start() || !text.is_char_boundary(p) {
                            continue;
                        }
                        let Some(c) = re.find_at(text, p) else { break };
                        if failed.contains(&c.start()) {
                            continue;
                        }
                        span = accepted(text, c.range(), d, re);
                        if span.is_some() {
                            break;
                        }
                        failed.insert(c.start());
                    }
                }
            }
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
/// a group of four (a grouped card's first), one of 13 to 19 digits (a card
/// written whole) or the candidate's last group (cut off by the length cap:
/// a whole card may follow), beginning with a digit some network issues (2
/// to 6). At most eight.
fn later_groups(text: &str, span: &Range<usize>) -> Vec<usize> {
    let g = groups(text, span);
    let last = g.len().saturating_sub(1);
    g.iter()
        .enumerate()
        .skip(1)
        .filter(|(i, r)| {
            (r.len() == 4 || r.len() >= 13 || *i == last)
                && (b'2'..=b'6').contains(&text.as_bytes()[r.start])
        })
        .take(8)
        .map(|(_, r)| r.start)
        .collect()
}
/// Inside a failed IBAN candidate: each later position where a registered
/// country code and two check digits start (an earlier token may have
/// started the candidate: `Ref12 DE89 …`, `ab12DE89…`). At most eight.
fn later_tokens(text: &str, span: &Range<usize>) -> Vec<usize> {
    let b = text.as_bytes();
    (span.start + 1..span.end)
        .filter(|&p| {
            b.get(p..p + 4).is_some_and(|t| {
                t[2..].iter().all(u8::is_ascii_digit)
                    && IBAN_LENGTHS
                        .iter()
                        .any(|(c, _)| c.as_bytes().eq_ignore_ascii_case(&t[..2]))
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
/// The candidate's leading whole groups that hold 13 to 19 digits, longest
/// first (a card followed by an expiry or a security code). A card that
/// starts at a later group is found by the retry there (`later_groups`).
/// A card has at most five groups, so a candidate of more than eight (a
/// list of short numbers) has none worth trying: the work per candidate
/// stays small whatever the text.
fn group_runs(text: &str, span: &Range<usize>) -> Vec<Range<usize>> {
    let g = groups(text, span);
    // Every network's numbers start with 2 to 6.
    if g.len() > 8 || !(b'2'..=b'6').contains(&text.as_bytes()[span.start]) {
        return Vec::new();
    }
    let mut digits = 0;
    let mut runs: Vec<Range<usize>> = g
        .iter()
        .filter_map(|r| {
            digits += r.len();
            (13..=19).contains(&digits).then(|| span.start..r.end)
        })
        .collect();
    // The whole candidate was tried already.
    runs.retain(|r| r.end != span.end);
    runs.reverse();
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

#[cfg(test)]
#[path = "personal_tests.rs"]
mod tests;
