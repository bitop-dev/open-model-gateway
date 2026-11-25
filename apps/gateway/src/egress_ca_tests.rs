//! The trust-anchor file's checks: a bundle is PEM certificates only, each a
//! parseable X.509 anchor, at least one; every refusal names the setting and
//! never the file's contents.
use super::CaBundle;
use crate::identity::test_pki::{Ca, Issued, San, Spec};

const SETTING: &str = "GATEWAY_OIDC_CA_FILE";

/// A PEM section with this label, assembled at run time so the source holds
/// no key marker for the secret scanner (these sections hold no key).
fn section(label: &[&str], body: &str) -> String {
    let label = label.join(" ");
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
}

fn refusal(pem: &[u8]) -> String {
    CaBundle::parse(SETTING, pem).err().unwrap().to_string()
}

#[test]
fn certificates_are_accepted_alone_or_together() {
    let (a, b) = (Ca::new("OMG test CA"), Ca::new("OMG other CA"));
    let one = CaBundle::parse(SETTING, a.certificate.pem().as_bytes()).unwrap();
    assert_eq!(one.certificates().len(), 1);
    // Text around the sections (comments) is ignored, as PEM allows.
    let both = format!(
        "# private roots\n{}\n{}",
        a.certificate.pem(),
        b.certificate.pem()
    );
    assert_eq!(
        CaBundle::parse(SETTING, both.as_bytes())
            .unwrap()
            .certificates()
            .len(),
        2
    );
}

#[test]
fn every_other_file_is_refused_naming_the_setting_only() {
    let ca = Ca::new("OMG test CA");
    let key = section(&["PRIVATE", "KEY"], &base64_of(&ca.certificate.key_pkcs8));
    let not_der = Issued {
        der: b"not a certificate at all, but valid base64".to_vec(),
        key_pkcs8: Vec::new(),
    }
    .pem();
    let cases: [(&str, Vec<u8>, &str); 6] = [
        ("empty", Vec::new(), "contains no certificate"),
        (
            "text",
            b"just some text\n".to_vec(),
            "contains no certificate",
        ),
        (
            "a private key",
            key.clone().into_bytes(),
            "must contain only PEM certificates",
        ),
        (
            "a certificate and a key",
            format!("{}{key}", ca.certificate.pem()).into_bytes(),
            "must contain only PEM certificates",
        ),
        (
            "broken base64",
            b"-----BEGIN CERTIFICATE-----\n!invalid-base64!\n-----END CERTIFICATE-----\n".to_vec(),
            "is not a PEM certificate bundle",
        ),
        (
            "not DER",
            not_der.into_bytes(),
            "is not a PEM certificate bundle",
        ),
    ];
    for (what, pem, message) in cases {
        let error = refusal(&pem);
        assert_eq!(error, format!("{SETTING} {message}"), "{what}");
    }
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.pem");
    for path in [missing.to_str().unwrap(), dir.path().to_str().unwrap()] {
        let error = CaBundle::read(SETTING, path).err().unwrap().to_string();
        assert_eq!(error, format!("{SETTING} could not be read"));
        assert!(!error.contains(path));
    }
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Every entry becomes a trust anchor, whose CA flag and validity the TLS
/// verifier never checks: a server certificate, an expired root and one not
/// yet valid are refused when the file is read.
#[test]
fn only_ca_certificates_within_their_validity_are_roots() {
    let ca = Ca::new("OMG test CA");
    let leaf = ca.issue(&Spec {
        common_name: "issuer",
        from: 2020,
        until: 2120,
        sans: &[San::Dns("issuer.example.test"), San::Ip([10, 0, 0, 1])],
    });
    assert_eq!(
        refusal(leaf.pem().as_bytes()),
        format!("{SETTING} must contain only CA certificates, not a server certificate")
    );
    assert_eq!(
        refusal(format!("{}{}", ca.certificate.pem(), leaf.pem()).as_bytes()),
        format!("{SETTING} must contain only CA certificates, not a server certificate")
    );
    for (from, until) in [(2000, 2010), (2100, 2110)] {
        let old = Ca::from_spec(&Spec {
            common_name: "OMG dated CA",
            from,
            until,
            sans: &[],
        });
        assert_eq!(
            refusal(old.certificate.pem().as_bytes()),
            format!("{SETTING} contains a certificate outside its validity period"),
            "{from}-{until}"
        );
    }
    // Both time encodings and the boundary itself.
    let at = |now: &str| CaBundle::parse_at(SETTING, ca.certificate.pem().as_bytes(), now);
    assert!(at("20200101000000Z").is_ok(), "valid from its first second");
    assert!(at("20191231235959Z").is_err());
    assert!(at("21191231235959Z").is_ok());
    assert!(at("21200101000000Z").is_err(), "not at its last instant");
}

/// The PEM reader skips sections it does not know and lines that do not
/// start with the marker; none of them may hide in a bundle.
#[test]
fn no_section_is_passed_over_unread() {
    let pem = Ca::new("OMG test CA").certificate.pem();
    let body: Vec<&str> = pem.lines().collect();
    let indented: String = body.iter().map(|l| format!("  {l}\n")).collect();
    for (what, extra) in [
        (
            "an encrypted key",
            section(&["ENCRYPTED", "PRIVATE", "KEY"], "AAAA"),
        ),
        (
            "an OpenSSL trusted certificate",
            section(&["TRUSTED", "CERTIFICATE"], "AAAA"),
        ),
        ("an indented certificate", indented),
        (
            "a marker inside a line",
            format!("note -----BEGIN CERTIFICATE-----\n{}", body[1]),
        ),
    ] {
        let bundle = format!("{pem}{extra}");
        let error = refusal(bundle.as_bytes());
        assert!(
            error == format!("{SETTING} must contain only PEM certificates"),
            "{what}: {error}"
        );
    }
}

#[test]
fn an_oversized_file_is_refused_unread() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.pem");
    std::fs::write(&path, vec![b'#'; 1024 * 1024 + 1]).unwrap();
    let error = CaBundle::read(SETTING, path.to_str().unwrap())
        .err()
        .unwrap()
        .to_string();
    assert_eq!(error, format!("{SETTING} is larger than 1 MiB"));
}

#[test]
fn now_is_generalized_time() {
    let now = super::generalized_now();
    assert_eq!(now.len(), 15, "{now}");
    assert!(now.ends_with('Z') && now[..14].bytes().all(|b| b.is_ascii_digit()));
    assert!(now.as_str() > "20260101000000Z");
}
