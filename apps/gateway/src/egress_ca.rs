//! Operator-configured extra trust anchors for one outbound client: the file
//! store's S3 endpoint (`GATEWAY_S3_CA_FILE`) and the OIDC issuer
//! (`GATEWAY_OIDC_CA_FILE`). A bundle is given to the one client its setting
//! names ([`super::add_root_certificates`]); the roots are added to the public
//! webpki roots, never instead of them, and no other client sees them. It is
//! read once, at startup, so rotating the CA needs a restart.
//!
//! The two settings accept different files, deliberately: the OIDC bundle
//! goes through [`CaBundle::read`]'s checks (CA certificates only), while S3
//! keeps exactly the rules it has always had ([`CaBundle::pem_bundle`]), so an
//! existing installation is never stopped at upgrade (for example one whose
//! endpoint has a single self-signed server certificate, named as its own
//! trust anchor, or a combined file that also holds the key).

/// Certificates of one setting's PEM file. Holds no secret (certificates are
/// public), but is never printed: errors name the setting, not the contents.
pub(crate) struct CaBundle {
    certificates: Vec<reqwest::Certificate>,
}

/// A bundle file larger than this is refused (a CA bundle is kilobytes).
const MAX_BYTES: u64 = 1024 * 1024;

impl CaBundle {
    /// Reads `path` (the value of `setting`): at most 1 MiB of PEM
    /// `CERTIFICATE` sections and nothing else, at least one, each a
    /// well-formed X.509 CA certificate (`basicConstraints` cA) within its
    /// validity now. Each becomes a trust anchor for the client its setting
    /// names. A missing, unreadable, empty or oversized file, any other PEM
    /// section (a private key, a request, an indented or unknown label), a
    /// certificate that does not parse, a server certificate and an expired
    /// or not yet valid certificate all fail, naming the setting and never the
    /// path or contents.
    pub(crate) fn read(setting: &'static str, path: &str) -> anyhow::Result<Self> {
        use std::io::Read as _;
        let unreadable = || anyhow::anyhow!("{setting} could not be read");
        let mut pem = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut pem))
            .map_err(|_| unreadable())?;
        anyhow::ensure!(
            pem.len() as u64 <= MAX_BYTES,
            "{setting} is larger than 1 MiB"
        );
        Self::parse(setting, &pem)
    }

    pub(crate) fn parse(setting: &'static str, pem: &[u8]) -> anyhow::Result<Self> {
        Self::parse_at(setting, pem, &generalized_now())
    }

    /// [`Self::parse`] at `now`, a GeneralizedTime (`YYYYMMDDHHMMSSZ`).
    fn parse_at(setting: &'static str, pem: &[u8], now: &str) -> anyhow::Result<Self> {
        use rustls::pki_types::{CertificateDer, pem::SectionKind};
        let invalid = || anyhow::anyhow!("{setting} is not a PEM certificate bundle");
        // The PEM parser skips sections it does not know and lines that do
        // not start with the marker: count every BEGIN marker anywhere, so
        // nothing in the file is passed over unread.
        let mut sections = 0;
        for line in pem.split(|b| *b == b'\n') {
            let marker = line.windows(10).position(|w| w == b"-----BEGIN");
            if let Some(at) = marker {
                if at != 0 || line.trim_ascii_end() != b"-----BEGIN CERTIFICATE-----" {
                    anyhow::bail!("{setting} must contain only PEM certificates");
                }
                sections += 1;
            }
        }
        let mut reader = std::io::Cursor::new(pem);
        let mut anchors = rustls::RootCertStore::empty();
        let mut certificates = Vec::new();
        while let Some((kind, der)) =
            rustls::pki_types::pem::from_buf(&mut reader).map_err(|_| invalid())?
        {
            if kind != SectionKind::Certificate {
                anyhow::bail!("{setting} must contain only PEM certificates");
            }
            anchors
                .add(CertificateDer::from(der.as_slice()))
                .map_err(|_| invalid())?;
            // The verifier uses an anchor's subject and key only: it never
            // checks an anchor's CA flag or validity, so check them here.
            let anchor = anchor(&der).ok_or_else(invalid)?;
            anyhow::ensure!(
                anchor.ca,
                "{setting} must contain only CA certificates, not a server certificate"
            );
            anyhow::ensure!(
                anchor.not_before.as_str() <= now && now < anchor.not_after.as_str(),
                "{setting} contains a certificate outside its validity period"
            );
            certificates.push(reqwest::Certificate::from_der(&der).map_err(|_| invalid())?);
        }
        anyhow::ensure!(
            !certificates.is_empty(),
            "{setting} contains no certificate"
        );
        anyhow::ensure!(certificates.len() == sections, invalid());
        Ok(Self { certificates })
    }

    /// What `reqwest::Certificate::from_pem_bundle` accepts and nothing more,
    /// for `GATEWAY_S3_CA_FILE`: its `CERTIFICATE` sections, other sections
    /// ignored, no CA-flag, validity or size check. Never use it for a new
    /// setting; `read` is the rule for those.
    pub(crate) fn pem_bundle(setting: &'static str, pem: &[u8]) -> anyhow::Result<Self> {
        let certificates = reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|_| anyhow::anyhow!("{setting} is not a PEM certificate bundle"))?;
        Ok(Self { certificates })
    }

    pub(crate) fn certificates(&self) -> &[reqwest::Certificate] {
        &self.certificates
    }
}

/// What the startup check reads of an anchor certificate.
struct Anchor {
    ca: bool,
    /// GeneralizedTime (`YYYYMMDDHHMMSSZ`), comparable as text.
    not_before: String,
    not_after: String,
}

/// One DER element: (tag, contents, the rest). Definite lengths only.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let (bytes, rest) = rest.split_at(n);
        (
            bytes.iter().fold(0usize, |v, b| (v << 8) | usize::from(*b)),
            rest,
        )
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}
fn elements(mut input: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    while !input.is_empty() {
        let (tag, contents, rest) = element(input)?;
        out.push((tag, contents));
        input = rest;
    }
    Some(out)
}
/// An RFC 5280 time as GeneralizedTime text: UTCTime years 50-99 are 19xx.
fn time(tag: u8, value: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(value).ok()?;
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    match (tag, text.len()) {
        (0x17, 13) if text.ends_with('Z') && digits(&text[..12]) => Some(format!(
            "{}{text}",
            if &text[..2] < "50" { "20" } else { "19" }
        )),
        (0x18, 15) if text.ends_with('Z') && digits(&text[..14]) => Some(text.to_owned()),
        _ => None,
    }
}
/// The CA flag and validity of a certificate (`None`: not one RFC 5280
/// `TBSCertificate` this reader understands).
fn anchor(der: &[u8]) -> Option<Anchor> {
    let (0x30, certificate, []) = element(der)? else {
        return None;
    };
    let (0x30, tbs, _) = element(certificate)? else {
        return None;
    };
    let fields = elements(tbs)?;
    // version [0], serial, signature, issuer, validity, ...: validity is the
    // first SEQUENCE of exactly two times.
    let validity = fields.iter().find_map(|(tag, contents)| {
        let times = (*tag == 0x30).then(|| elements(contents)).flatten()?;
        match times.as_slice() {
            [(a, x), (b, y)] => Some((time(*a, x)?, time(*b, y)?)),
            _ => None,
        }
    })?;
    let mut ca = false;
    if let Some((_, extensions)) = fields.iter().find(|(tag, _)| *tag == 0xA3) {
        let (0x30, list, []) = element(extensions)? else {
            return None;
        };
        for (_, extension) in elements(list)? {
            let parts = elements(extension)?;
            let [(0x06, oid), .., (0x04, value)] = parts.as_slice() else {
                return None;
            };
            // basicConstraints (2.5.29.19): SEQUENCE { cA BOOLEAN DEFAULT FALSE, ... }
            if *oid == [0x55, 0x1D, 0x13] {
                let (0x30, constraints, []) = element(value)? else {
                    return None;
                };
                ca = matches!(elements(constraints)?.first(), Some((0x01, [flag])) if *flag != 0);
            }
        }
    }
    Some(Anchor {
        ca,
        not_before: validity.0,
        not_after: validity.1,
    })
}
/// Now as GeneralizedTime: the digits of the RFC 3339 date and time
/// (`2026-11-24T21:00:00.123456Z` is `20261124210000Z`).
fn generalized_now() -> String {
    let rfc3339 =
        gateway_datetime_compat::format::utc_json(gateway_datetime_compat::UtcInstant::now());
    let digits: String = rfc3339
        .chars()
        .take(19)
        .filter(char::is_ascii_digit)
        .collect();
    format!("{digits}Z")
}

#[cfg(test)]
#[path = "egress_ca_tests.rs"]
mod tests;
