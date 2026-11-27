//! #841: the personal-data kinds. Positives are synthetic or published test
//! values (issuer test cards, the IBAN registry's examples, public resolver
//! addresses), never real data. The negative corpus measures false positives:
//! seeded, deterministic corpora (UUIDs, digests, base64, JWT shapes, numeric
//! text, digit runs) with asserted bounds per kind, and this repository's own
//! source and docs, measured and reported with no bound (an address added to
//! a doc never fails a build).
use super::*;
use crate::guardrails::detector::Part;

fn found(text: &str) -> Vec<(Kind, String)> {
    let mut input = Inspection::default();
    input.push(text, Part::Content);
    let f = findings(&input);
    f.0.iter()
        .map(|x| (x.kind.unwrap(), text[x.span.clone()].to_owned()))
        .collect()
}
fn kinds_in(text: &str) -> Vec<Kind> {
    found(text).into_iter().map(|(k, _)| k).collect()
}
#[track_caller]
fn hit(kind: Kind, value: &str) {
    for text in [
        value.to_owned(),
        format!("see {value} here"),
        format!("\"{value}\","),
        format!("({value})"),
    ] {
        assert_eq!(
            found(&text),
            [(kind, value.to_owned())],
            "{kind:?} in {text:?}"
        );
    }
}
#[track_caller]
fn miss(kind: Kind, text: &str) {
    assert!(
        !kinds_in(text).contains(&kind),
        "{kind:?} must not match {text:?}: {:?}",
        found(text)
    );
}

#[test]
fn the_kind_table_matches_the_migration() {
    // Migration 0099's list of kinds (rules and events) is this table's.
    let sql = include_str!("../../enterprise_migrations/0099_guardrail_personal_data_kinds.sql");
    let list = format!(
        "ARRAY[{}]",
        Kind::ALL
            .iter()
            .map(|k| format!("'{}'", k.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(sql.contains(&list), "rules list: {list}");
    let events = format!(
        "kind IN ({})",
        Kind::ALL
            .iter()
            .map(|k| format!("'{}'", k.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(sql.contains(&events), "events list: {events}");
    assert_eq!(KINDS.len(), Kind::ALL.len());
    for k in Kind::ALL {
        assert_eq!(def(*k).kind, *k);
        assert!(!def(*k).matches.is_empty() && !def(*k).label.is_empty());
    }
}

/// `text` holds `value` as a `kind` finding (among others it may hold).
#[track_caller]
fn holds(kind: Kind, text: &str, value: &str) {
    assert!(
        found(text).contains(&(kind, value.to_owned())),
        "{kind:?} {value:?} in {text:?}: {:?}",
        found(text)
    );
}

#[test]
fn the_dashboard_says_what_each_kind_matches_in_the_same_words() {
    let ts = include_str!("../../../web/src/lib/guardrails.ts");
    for d in KINDS {
        assert!(
            ts.contains(&format!("\"{}\"", d.matches)),
            "{:?}: {}",
            d.kind,
            d.matches
        );
    }
}

#[test]
fn values_next_to_other_text_are_still_found() {
    holds(
        Kind::Card,
        "4111 1111 1111 1111 12/27",
        "4111 1111 1111 1111",
    );
    holds(Kind::Card, "4242 4242 4242 4242 123", "4242 4242 4242 4242");
    holds(Kind::Card, "4111111111111111 12/27", "4111111111111111");
    holds(Kind::Card, "2 4111 1111 1111 1111", "4111 1111 1111 1111");
    holds(
        Kind::Phone,
        "+44 20 7946 0958. 3 people",
        "+44 20 7946 0958",
    );
    holds(Kind::Phone, "call 1-415-555-2671", "1-415-555-2671");
    holds(Kind::Phone, "or 1.415.555.2671", "1.415.555.2671");
    holds(
        Kind::Iban,
        "BE68 5390 0754 7034 SWIFT",
        "BE68 5390 0754 7034",
    );
    holds(
        Kind::Iban,
        "ES91 2100 0418 4502 0005 1332 BIC CAIXESBBXXX",
        "ES91 2100 0418 4502 0005 1332",
    );
    holds(
        Kind::Iban,
        "de89 3704 0044 0532 0130 00",
        "de89 3704 0044 0532 0130 00",
    );
    holds(Kind::Ip, "ip:8.8.8.8", "8.8.8.8");
    holds(Kind::Ip, "X-Forwarded-For:8.8.4.4", "8.8.4.4");
    holds(Kind::Ip, "client ::ffff:8.8.8.8 connected", "8.8.8.8");
    holds(
        Kind::Ip,
        "2001:4860:4860::8888: down",
        "2001:4860:4860::8888",
    );
    holds(
        Kind::Email,
        "andré.dupont@gmail.com",
        "andré.dupont@gmail.com",
    );
    holds(
        Kind::Email,
        "write to alice@acme.com: now",
        "alice@acme.com",
    );
    holds(Kind::Email, "user@gmail.com:hunter2", "user@gmail.com");
    holds(Kind::Email, "请联系alice@acme.io", "alice@acme.io");
    holds(
        Kind::Card,
        "Order 12345 4111 1111 1111 1111",
        "4111 1111 1111 1111",
    );
    holds(Kind::Card, "12345 4111111111111111", "4111111111111111");
    holds(Kind::Ip, "ip:2606:4700::1111", "2606:4700::1111");
    holds(
        Kind::Ip,
        "X-Forwarded-For:2001:4860:4860::8888",
        "2001:4860:4860::8888",
    );
    holds(Kind::Ip, "id:2606:4700::1111", "2606:4700::1111");
}

#[test]
fn one_kind_never_hides_or_crowds_out_another() {
    // A value two kinds match counts for both.
    holds(Kind::Card, "4111111111111111@pay.io", "4111111111111111");
    holds(
        Kind::Email,
        "4111111111111111@pay.io",
        "4111111111111111@pay.io",
    );
    // Each kind has its own cap: a request full of one kind's values still
    // has the other kinds' found.
    let text = format!(
        "{}4111 1111 1111 1111",
        "x@acme.io ".repeat(MAX_FINDINGS + 5)
    );
    let mut input = Inspection::default();
    input.push(&text, Part::Content);
    let f = findings(&input);
    assert_eq!(f.distinct_kind(&input, Kind::Card), 1);
    // A repeated value counts once and takes no room from distinct ones.
    assert_eq!(
        f.0.iter().filter(|x| x.kind == Some(Kind::Email)).count(),
        1
    );
    let many = format!(
        "{}{}",
        "x@acme.io ".repeat(MAX_FINDINGS + 5),
        (0..50)
            .map(|i| format!("u{i}@acme.io "))
            .collect::<String>()
    );
    let mut input = Inspection::default();
    input.push(&many, Part::Content);
    assert_eq!(findings(&input).distinct_kind(&input, Kind::Email), 51);
}

#[test]
fn emails() {
    hit(Kind::Email, "alice.smith@acme-corp.io");
    hit(Kind::Email, "a+tag@mail.co.uk");
    for text in [
        "user@example.com",
        "ops@sub.example.org",
        "x@service.test",
        "x@host.invalid",
        "x@app.localhost",
        "left-pad@1.3.0",
        "@types/node",
        "git@github.com:acme/repo.git",
        "nginx@sha256:abc",
        ".dot@acme.io",
        "a..b@acme.io",
    ] {
        miss(Kind::Email, text);
    }
}

#[test]
fn phones() {
    hit(Kind::Phone, "+1 415 555 2671");
    hit(Kind::Phone, "+44 20 7946 0958");
    hit(Kind::Phone, "+4915112345678");
    hit(Kind::Phone, "(415) 555-2671");
    hit(Kind::Phone, "415-555-2671");
    hit(Kind::Phone, "415.555.2671");
    for text in [
        "555-0123",
        "415-555-0123",
        "(415) 555-0199",
        "2026-11-27",
        "415-555.2671",
        "+1 234",
        "call +12  345 678 90",
        "id 1415-555-2671",
        "123-456-7890",
    ] {
        miss(Kind::Phone, text);
    }
}

#[test]
fn cards() {
    hit(Kind::Card, "4111 1111 1111 1111");
    hit(Kind::Card, "5555555555554444");
    hit(Kind::Card, "3782 822463 10005");
    hit(Kind::Card, "6011-1111-1111-1117");
    for text in [
        "4111111111111112",
        "1234567890123456",
        "4111-1111 1111-1111",
        "41111 1111 1111 111",
        "94111111111111111",
        "ref-4111111111111111",
        "4111111111111111_x",
    ] {
        miss(Kind::Card, text);
    }
}

#[test]
fn ibans() {
    hit(Kind::Iban, "DE89 3704 0044 0532 0130 00");
    hit(Kind::Iban, "GB82WEST12345698765432");
    hit(Kind::Iban, "FR1420041010050500013M02606");
    for text in [
        "DE89 3704 0044 0532 0130 01",
        "XX89370400440532013000",
        "DE8937040044053201300",
        "DE89 370 40044 0532 0130 00",
        "IDDE89370400440532013000",
    ] {
        miss(Kind::Iban, text);
    }
}

#[test]
fn us_ssns() {
    hit(Kind::UsSsn, "512-34-5678");
    hit(Kind::UsSsn, "512 34 5678");
    for text in [
        "000-12-3456",
        "666-12-3456",
        "912-34-5678",
        "512-00-5678",
        "512-34-0000",
        "078-05-1120",
        "219-09-9999",
        "512-34 5678",
        "512345678",
        "1512-34-5678",
        "512-34-56789",
    ] {
        miss(Kind::UsSsn, text);
    }
}

#[test]
fn ips_are_public_addresses_only() {
    hit(Kind::Ip, "8.8.8.8");
    hit(Kind::Ip, "1.1.1.1");
    hit(Kind::Ip, "2606:4700:4700::1111");
    assert_eq!(
        kinds_in("dial 8.8.8.8:53 or 1.1.1.1/32"),
        [Kind::Ip, Kind::Ip]
    );
    for text in [
        "10.0.0.1",
        "127.0.0.1",
        "192.168.1.1",
        "172.16.0.1",
        "169.254.1.1",
        "100.64.0.1",
        "198.18.0.1",
        "192.0.2.1",
        "198.51.100.7",
        "203.0.113.9",
        "224.0.0.1",
        "255.255.255.255",
        "0.0.0.0",
        "::1",
        "::",
        "fe80::1",
        "fd00::1",
        "ff02::1",
        "2001:db8::1",
        "3fff::1",
        "3fff:0fff::1",
        "2002:c000:0204::1",
        "v8.8.8.8",
        "8.8.8.8.5",
        "8.8.8.8-rc1",
        "300.1.1.1",
        "08.8.8.8",
        "std::io::Error",
        "12:30:45",
        "dead::beef",
    ] {
        miss(Kind::Ip, text);
    }
}

#[test]
fn a_value_counts_once_and_kinds_do_not_overlap() {
    let mut input = Inspection::default();
    input.push("8.8.8.8 and 8.8.8.8, mail alice@acme.io", Part::Content);
    input.push("alice@acme.io again", Part::Content);
    let f = findings(&input);
    assert_eq!(f.distinct_kind(&input, Kind::Ip), 1);
    assert_eq!(f.distinct_kind(&input, Kind::Email), 1);
    assert_eq!(f.distinct_kind(&input, Kind::Card), 0);
}

// --- False positives ---------------------------------------------------------------

/// A deterministic generator (xorshift64*), so the corpora are the same on
/// every run.
struct Seeded(u64);
impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn b64(b: &[u8], url: bool) -> String {
    use base64::Engine;
    if url {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    } else {
        base64::engine::general_purpose::STANDARD.encode(b)
    }
}

/// The seeded corpora: (name, items).
fn seeded() -> Vec<(&'static str, Vec<String>)> {
    let mut r = Seeded(0x0841_0841_0841_0841);
    let uuids = (0..20_000)
        .map(|_| {
            let h = hex(&r.bytes(16));
            format!(
                "{}-{}-4{}-a{}-{}",
                &h[..8],
                &h[8..12],
                &h[13..16],
                &h[17..20],
                &h[20..32]
            )
        })
        .collect();
    let digests = (0..10_000)
        .map(|i| hex(&r.bytes([20, 32, 64][i % 3])))
        .collect();
    let base64 = (0..10_000)
        .map(|i| b64(&r.bytes(12 + (i % 48)), i % 2 == 0))
        .collect();
    let jwts = (0..2_000)
        .map(|_| {
            format!(
                "eyJ{}.eyJ{}.{}",
                b64(&r.bytes(18), true),
                b64(&r.bytes(60), true),
                b64(&r.bytes(32), true)
            )
        })
        .collect();
    let mut numeric = Vec::new();
    for _ in 0..2_000 {
        let s = 1_600_000_000 + r.below(200_000_000);
        numeric.push(format!("{s}"));
        numeric.push(format!("{}", s * 1000 + r.below(1000)));
        numeric.push(format!(
            "2026-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            1 + r.below(12),
            1 + r.below(28),
            r.below(24),
            r.below(60),
            r.below(60),
            r.below(1000)
        ));
        numeric.push(format!("{}.{}.{}", r.below(30), r.below(40), r.below(200)));
        numeric.push(format!("{}.{:02} USD", r.below(1_000_000), r.below(100)));
        numeric.push(format!(
            "ports {},{},{}",
            1024 + r.below(60000),
            1024 + r.below(60000),
            1024 + r.below(60000)
        ));
        numeric.push(format!(
            "src/main.rs:{}:{}",
            1 + r.below(5000),
            1 + r.below(120)
        ));
        numeric.push(format!(
            "HTTP/1.1 {} in {} ms",
            200 + r.below(400),
            r.below(100000)
        ));
    }
    let digit_runs = (0..10_000)
        .map(|_| {
            let n = 13 + r.below(7) as usize;
            (0..n)
                .map(|i| {
                    char::from(b'0' + (if i == 0 { 1 + r.below(9) } else { r.below(10) }) as u8)
                })
                .collect()
        })
        .collect();
    vec![
        ("uuids", uuids),
        ("hex digests", digests),
        ("base64", base64),
        ("jwt shapes", jwts),
        ("numeric text", numeric),
        ("digit runs (13-19 digits)", digit_runs),
    ]
}

/// Findings per kind in `items` (each item a segment of its own).
fn measure(items: &[String]) -> std::collections::BTreeMap<Kind, usize> {
    let mut by = std::collections::BTreeMap::new();
    for chunk in items.chunks(500) {
        let mut input = Inspection::default();
        for item in chunk {
            input.push(item, Part::Content);
        }
        for f in findings(&input).0 {
            *by.entry(f.kind.unwrap()).or_insert(0) += 1;
        }
    }
    by
}

/// The asserted bounds: false positives per 10,000 items, per kind and
/// corpus. Zero everywhere except card numbers among random digit runs,
/// where an issuer prefix at an issued length and a valid check digit happen
/// by chance (measured 118 in 10,000).
fn bound(corpus: &str, kind: Kind) -> usize {
    match (corpus, kind) {
        ("digit runs (13-19 digits)", Kind::Card) => 200,
        _ => 0,
    }
}

#[test]
fn false_positives_stay_within_their_bounds_on_the_seeded_corpora() {
    let mut report = String::from(
        "corpus | items | email | phone | card | iban | us_ssn | ip (per 10k items)\n",
    );
    let mut over = Vec::new();
    for (name, items) in seeded() {
        let by = measure(&items);
        let per = |k: Kind| by.get(&k).copied().unwrap_or(0) * 10_000 / items.len().max(1);
        report.push_str(&format!(
            "{name} | {} | {} | {} | {} | {} | {} | {}\n",
            items.len(),
            per(Kind::Email),
            per(Kind::Phone),
            per(Kind::Card),
            per(Kind::Iban),
            per(Kind::UsSsn),
            per(Kind::Ip)
        ));
        for k in Kind::ALL {
            if per(*k) > bound(name, *k) {
                over.push(format!(
                    "{name}: {k:?} {} per 10k > {}",
                    per(*k),
                    bound(name, *k)
                ));
            }
        }
    }
    eprintln!("{report}");
    assert!(over.is_empty(), "{over:?}\n{report}");
}

/// This repository's source and docs, measured and reported (per MB), never
/// bounded: real text where an example address or a version string may
/// appear at any time.
#[test]
fn the_repository_source_and_docs_are_measured() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    for (dir, exts) in [
        ("apps/gateway/src", &["rs"][..]),
        ("apps/web/src", &["ts", "tsx"][..]),
        ("docs", &["md"][..]),
    ] {
        let mut stack = vec![root.join(dir)];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| exts.contains(&x))
                    && let Ok(text) = std::fs::read_to_string(&p)
                {
                    sources.push((dir, text));
                }
            }
        }
    }
    let mut report =
        String::from("source | MB | email | phone | card | iban | us_ssn | ip (findings per MB)\n");
    for dir in ["apps/gateway/src", "apps/web/src", "docs"] {
        let texts: Vec<String> = sources
            .iter()
            .filter(|(d, _)| *d == dir)
            .map(|(_, t)| t.clone())
            .collect();
        let bytes: usize = texts.iter().map(String::len).sum();
        let by = measure(&texts);
        let mb = bytes as f64 / 1_000_000.0;
        let per = |k: Kind| by.get(&k).copied().unwrap_or(0) as f64 / mb.max(0.001);
        report.push_str(&format!(
            "{dir} | {mb:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1}\n",
            per(Kind::Email),
            per(Kind::Phone),
            per(Kind::Card),
            per(Kind::Iban),
            per(Kind::UsSsn),
            per(Kind::Ip)
        ));
        assert!(bytes > 0, "{dir} read");
    }
    eprintln!("{report}");
}
