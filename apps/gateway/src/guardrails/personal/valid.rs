//! The personal-data validators: what keeps only well-formed values of a
//! kind once its pattern found a candidate (a checksum where the format has
//! one, the issued ranges, the routable address space).
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::sync::LazyLock;

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
pub(super) fn email(v: &str) -> bool {
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

pub(super) fn phone(v: &str) -> bool {
    // `00` stands for `+`; written without a separator it is any number
    // that starts with two zeros.
    let international = v.strip_prefix('+').or_else(|| {
        v.strip_prefix("00")
            .filter(|r| r.bytes().any(|b| b" -.()".contains(&b)))
    });
    if let Some(rest) = international {
        // A `00` number in groups of four only is a row of four-digit
        // numbers (a table, a hex dump), as a card or an IBAN is written.
        if v.starts_with("00")
            && v.split(|c: char| !c.is_ascii_digit())
                .filter(|g| !g.is_empty())
                .all(|g| g.len() == 4)
        {
            return false;
        }
        let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
        // A date after a `+` (an added line in a diff: `+2026-11-27`,
        // `+20240101`, `+2026-11-27 10:00:00`) is no number.
        if leading_date(rest) {
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
    // Two zeros without a separator start any number (a zero-padded
    // identifier, `0090012345`): no phone.
    if v.starts_with("00") {
        return false;
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
    !seps.is_empty() && seps.windows(2).all(|w| w[0] == w[1]) && !fictional
}

/// Whether `rest` starts with a date: `YYYY-MM-DD`, `YYYY.MM.DD` or
/// `YYYYMMDD` not followed by another digit, from 1900 to 2099.
fn leading_date(rest: &str) -> bool {
    let b = rest.as_bytes();
    let digit = |r: Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    if digit(0..4) && digit(5..7) && digit(8..10) && b[4] == b[7] && b"-.".contains(&b[4]) {
        let d = format!("{}{}{}", &rest[..4], &rest[5..7], &rest[8..10]);
        return date(&d) && !b.get(10).is_some_and(u8::is_ascii_digit);
    }
    digit(0..8) && !b.get(8).is_some_and(u8::is_ascii_digit) && date(&rest[..8])
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
pub(super) fn card(v: &str) -> bool {
    let b = v.as_bytes();
    // Every network's numbers start with 2 to 6: most candidates stop here.
    if !(b'2'..=b'6').contains(&b[0]) {
        return false;
    }
    // Without allocating: the digits (at most 19) and the group lengths.
    let mut digits = [0u8; 19];
    let (mut n, mut sep, mut groups, mut g) = (0, None, [0usize; 19], 0);
    for &c in b {
        if c.is_ascii_digit() {
            if n == 19 {
                return false;
            }
            digits[n] = c;
            n += 1;
            groups[g] += 1;
        } else {
            if sep.is_some_and(|s| s != c) {
                return false;
            }
            sep = Some(c);
            g += 1;
            if g == 19 {
                return false;
            }
        }
    }
    if n < 13 {
        return false;
    }
    let groups = &groups[..=g];
    if sep.is_some() {
        // Written in groups: 4-4-4-4(-3), or 4-6-5 / 4-6-4 (Amex, Diners).
        let fours = groups[..groups.len() - 1].iter().all(|g| *g == 4)
            && (1..=4).contains(&groups[groups.len() - 1]);
        if !(fours || groups == [4, 6, 5] || groups == [4, 6, 4]) {
            return false;
        }
    }
    let digits = &digits[..n];
    // ASCII digits only, so always valid UTF-8.
    std::str::from_utf8(digits).is_ok_and(issued) && luhn(digits)
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
pub(super) static IBAN_LENGTHS: LazyLock<Vec<(&'static str, usize)>> = LazyLock::new(|| {
    IBAN_REGISTRY
        .split_whitespace()
        .map(|e| (&e[..2], e[2..].parse().expect("an IBAN length")))
        .collect()
});
pub(super) fn iban(v: &str) -> bool {
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

pub(super) fn ssn(v: &str) -> bool {
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
fn global_v4(a: Ipv4Addr) -> bool {
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
fn global_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        && !(s[0] == 0x2001 && s[1] < 0x0200)
        && s[0] != 0x2002
        // 3fff::/20, documentation since 2024 (RFC 9637).
        && !(s[0] == 0x3fff && s[1] < 0x1000)
}
pub(super) fn ip(v: &str) -> bool {
    if v.contains('.') {
        // No leading zeros: `010.1.1.1` is not written by anyone meaning an address.
        if v.split('.').any(|o| o.len() > 1 && o.starts_with('0')) {
            return false;
        }
        return v.parse::<Ipv4Addr>().is_ok_and(global_v4);
    }
    v.parse::<Ipv6Addr>().is_ok_and(global_v6)
}
