//! The personal-data kinds of the pattern detector (#841,
//! docs/guardrails-design.md §2). Each kind is one entry of [`KINDS`]: a
//! candidate pattern (on the `regex` crate: linear time), a validator that
//! keeps only well-formed values (a checksum where the format has one), and
//! the characters that may not touch a match on either side, so a value
//! inside a longer token (an identifier, a hash, a longer number) is not
//! one. Adding a format, such as another country's national identifier, is
//! one entry here and one value in migration 0098's list.
//!
//! What is deliberately not personal data, and never matches: addresses
//! that are not globally routable (loopback, private, link-local, shared,
//! benchmarking, multicast, broadcast, reserved and documentation ranges),
//! the reserved example domains (`example.com`, `.test`, `.invalid`, …),
//! fictional `555-01xx` numbers and the advertised sample SSNs. Nothing found
//! is kept beyond the request: a finding is a segment index and a span.
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use regex::Regex;

use super::detector::{Finding, Findings, Inspection};
use super::policy::{Category, Kind};
use super::secrets::MAX_FINDINGS;

/// One personal-data kind.
pub struct KindDef {
    pub kind: Kind,
    /// The redaction label (`[REDACTED:<LABEL>_<n>]`, #841 redact).
    pub label: &'static str,
    /// What it matches, in one plain sentence (docs and the dashboard).
    pub matches: &'static str,
    pattern: &'static str,
    /// Characters that may not come right before a match.
    before: fn(u8) -> bool,
    /// Characters that may not come right after a match.
    after: fn(u8) -> bool,
    valid: fn(&str) -> bool,
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The kinds, in the order overlapping matches are resolved (an earlier kind
/// wins a span both match; matches of different kinds rarely overlap).
pub const KINDS: &[KindDef] = &[
    KindDef {
        kind: Kind::Email,
        label: "EMAIL",
        matches: "Email addresses, except at reserved example domains (example.com, .test, .invalid, .localhost).",
        pattern: r"[A-Za-z0-9._%+\-]{1,64}@(?:[A-Za-z0-9](?:[A-Za-z0-9\-]{0,61}[A-Za-z0-9])?\.){1,8}[A-Za-z]{2,63}",
        before: |b| alnum(b) || b".%+-_".contains(&b),
        // `git@github.com:org/repo` is an SSH remote, not an address.
        after: |b| alnum(b) || b"-_:".contains(&b),
        valid: email,
    },
    KindDef {
        kind: Kind::Phone,
        label: "PHONE",
        matches: "Phone numbers in international form (+ and 8 to 15 digits) or North American form with separators ((415) 555-2671, 415-555-2671); fictional 555-01xx numbers excepted.",
        pattern: r"\+[1-9][0-9 ()\-.]{6,22}[0-9]|(?:\([2-9][0-9]{2}\) ?|[2-9][0-9]{2}[\-. ])[2-9][0-9]{2}[\-. ][0-9]{4}",
        before: |b| alnum(b) || b"+-./_".contains(&b),
        after: |b| alnum(b) || b"-/_".contains(&b),
        valid: phone,
    },
    KindDef {
        kind: Kind::Card,
        label: "CARD",
        matches: "Payment card numbers of the major networks: 13 to 19 digits, whole or in groups, with a valid check digit (Luhn).",
        pattern: r"[0-9](?:[ \-]?[0-9]){12,18}",
        before: |b| word(b) || b"-.".contains(&b),
        after: |b| word(b) || b == b'-',
        valid: card,
    },
    KindDef {
        kind: Kind::Iban,
        label: "IBAN",
        matches: "International bank account numbers (IBAN) with the length of their country and a valid check (mod 97).",
        pattern: r"[A-Z]{2}[0-9]{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,3})?",
        before: alnum,
        after: alnum,
        valid: iban,
    },
    KindDef {
        kind: Kind::UsSsn,
        label: "US_SSN",
        matches: "US Social Security numbers written with dashes or spaces (123-45-6789), within the issued ranges. Other countries' identifiers are not built in: use a custom pattern.",
        pattern: r"[0-9]{3}[\- ][0-9]{2}[\- ][0-9]{4}",
        before: |b| word(b) || b"-.".contains(&b),
        after: |b| word(b) || b == b'-',
        valid: ssn,
    },
    KindDef {
        kind: Kind::Ip,
        label: "IP",
        matches: "Public IP addresses (IPv4 and IPv6) only: never loopback, private, link-local, shared, multicast, reserved or documentation addresses.",
        pattern: r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}|[0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7}",
        // A version (`v1.2.3.4`) is glued to its letter; a range
        // (`1.2.3.4-5`) to its dash. A port (`8.8.8.8:53`) may follow.
        before: |b| word(b) || b".:".contains(&b),
        after: |b| word(b) || b == b'-',
        valid: ip,
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
fn standalone(text: &[u8], span: &std::ops::Range<usize>, d: &KindDef) -> bool {
    let before = span.start.checked_sub(1).map(|i| text[i]);
    let after = text.get(span.end).copied();
    if before.is_some_and(d.before) || after.is_some_and(d.after) {
        return false;
    }
    !(after == Some(b'.') && text.get(span.end + 1).is_some_and(u8::is_ascii_digit))
}

/// Every personal-data finding of every kind in `input` (shared by every
/// personal-data rule of one request; each counts only its own kinds).
pub fn findings(input: &Inspection<'_>) -> Findings {
    let mut found = Vec::new();
    for (index, segment) in input.segments.iter().enumerate() {
        let text = segment.text.as_bytes();
        let mut taken: Vec<std::ops::Range<usize>> = Vec::new();
        for (d, re) in KINDS.iter().zip(COMPILED.iter()) {
            for m in re.find_iter(&segment.text) {
                let span = m.range();
                if !standalone(text, &span, d)
                    || !(d.valid)(m.as_str())
                    || taken
                        .iter()
                        .any(|t| t.start < span.end && span.start < t.end)
                {
                    continue;
                }
                if found.len() == MAX_FINDINGS {
                    return Findings(found);
                }
                taken.push(span.clone());
                found.push(Finding {
                    category: Category::PersonalData,
                    kind: Some(d.kind),
                    segment: index,
                    span,
                });
            }
        }
    }
    Findings(found)
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
    !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && !reserved_domain(domain)
}

fn phone(v: &str) -> bool {
    let digits: String = v.chars().filter(char::is_ascii_digit).collect();
    if let Some(rest) = v.strip_prefix('+') {
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
    // North American: NPA-NXX-XXXX with one kind of separator, the
    // fictional 555-0100 to 555-0199 excepted.
    if v.starts_with('(') {
        let line = &digits[3..];
        return !(line.starts_with("555") && line[3..].starts_with("01"));
    }
    let seps: Vec<u8> = v.bytes().filter(|b| !b.is_ascii_digit()).collect();
    seps.windows(2).all(|w| w[0] == w[1])
        && !(digits[3..6] == *"555" && digits[6..].starts_with("01"))
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
/// Whether `digits` (a number of `len` digits) starts with a major
/// network's issuer prefix at a length that network issues.
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
}
fn card(v: &str) -> bool {
    let digits: String = v.chars().filter(char::is_ascii_digit).collect();
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

/// IBAN lengths by country (the SWIFT IBAN registry).
const IBAN_LENGTHS: &[(&str, usize)] = &[
    ("AD", 24),
    ("AE", 23),
    ("AL", 28),
    ("AT", 20),
    ("AZ", 28),
    ("BA", 20),
    ("BE", 16),
    ("BG", 22),
    ("BH", 22),
    ("BI", 27),
    ("BR", 29),
    ("BY", 28),
    ("CH", 21),
    ("CR", 22),
    ("CY", 28),
    ("CZ", 24),
    ("DE", 22),
    ("DJ", 27),
    ("DK", 18),
    ("DO", 28),
    ("EE", 20),
    ("EG", 29),
    ("ES", 24),
    ("FI", 18),
    ("FK", 18),
    ("FO", 18),
    ("FR", 27),
    ("GB", 22),
    ("GE", 22),
    ("GI", 23),
    ("GL", 18),
    ("GR", 27),
    ("GT", 28),
    ("HR", 21),
    ("HU", 28),
    ("IE", 22),
    ("IL", 23),
    ("IQ", 23),
    ("IS", 26),
    ("IT", 27),
    ("JO", 30),
    ("KW", 30),
    ("KZ", 20),
    ("LB", 28),
    ("LC", 32),
    ("LI", 21),
    ("LT", 20),
    ("LU", 20),
    ("LV", 21),
    ("LY", 25),
    ("MC", 27),
    ("MD", 24),
    ("ME", 22),
    ("MK", 19),
    ("MN", 20),
    ("MR", 27),
    ("MT", 31),
    ("MU", 30),
    ("NI", 28),
    ("NL", 18),
    ("NO", 15),
    ("OM", 23),
    ("PK", 24),
    ("PL", 28),
    ("PS", 29),
    ("PT", 25),
    ("QA", 29),
    ("RO", 24),
    ("RS", 22),
    ("RU", 33),
    ("SA", 24),
    ("SC", 31),
    ("SD", 18),
    ("SE", 24),
    ("SI", 19),
    ("SK", 24),
    ("SM", 27),
    ("SO", 23),
    ("ST", 25),
    ("SV", 28),
    ("TL", 23),
    ("TN", 24),
    ("TR", 26),
    ("UA", 29),
    ("VA", 22),
    ("VG", 24),
    ("XK", 20),
    ("YE", 30),
];
fn iban(v: &str) -> bool {
    let compact: String = v.chars().filter(|c| *c != ' ').collect();
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
    if b[3] != b[6] {
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
/// outside the documentation (2001:db8::/32), IETF protocol (2001::/23) and
/// 6to4 (2002::/16) ranges.
pub fn global_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        && !(s[0] == 0x2001 && s[1] < 0x0200)
        && s[0] != 0x2002
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
