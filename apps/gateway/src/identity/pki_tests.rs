//! Test-only PKI, generated when a test runs (no key is ever committed): a
//! P-256 CA and leaf certificates it signs, with the validity, names and
//! addresses each test asks for. A minimal X.509 v3 DER writer: version,
//! serial, ECDSA-SHA256, a common-name subject and issuer, validity, the
//! public key, basic constraints, key usage, server-auth extended key usage
//! and subject alternative names.
use base64::Engine as _;
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}
fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}
fn integer(value: &[u8]) -> Vec<u8> {
    let mut v: Vec<u8> = value.iter().copied().skip_while(|b| *b == 0).collect();
    if v.is_empty() || v[0] & 0x80 != 0 {
        v.insert(0, 0);
    }
    tlv(0x02, &v)
}
fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![0];
    v.extend_from_slice(bytes);
    tlv(0x03, &v)
}
const ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const P256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
fn name(common_name: &str) -> Vec<u8> {
    seq(&[tlv(
        0x31,
        &seq(&[
            vec![0x06, 0x03, 0x55, 0x04, 0x03],
            tlv(0x0C, common_name.as_bytes()),
        ]),
    )])
}
/// UTCTime before 2050, GeneralizedTime from then on (RFC 5280).
fn time(year: i32) -> Vec<u8> {
    if year < 2050 {
        tlv(0x17, format!("{:02}0101000000Z", year % 100).as_bytes())
    } else {
        tlv(0x18, format!("{year:04}0101000000Z").as_bytes())
    }
}
fn extension(oid: &[u8], critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![tlv(0x06, oid)];
    if critical {
        parts.push(vec![0x01, 0x01, 0xFF]);
    }
    parts.push(tlv(0x04, &value));
    seq(&parts)
}

/// One certificate and its private key.
pub(crate) struct Issued {
    pub(crate) der: Vec<u8>,
    pub(crate) key_pkcs8: Vec<u8>,
}
impl Issued {
    pub(crate) fn pem(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.der);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            lines.join("\n")
        )
    }
}

/// A subject alternative name.
pub(crate) enum San<'a> {
    Dns(&'a str),
    Ip([u8; 4]),
}

/// A certificate to write: `from`/`until` are years (1 January, 00:00 UTC).
pub(crate) struct Spec<'a> {
    pub(crate) common_name: &'a str,
    pub(crate) from: i32,
    pub(crate) until: i32,
    pub(crate) sans: &'a [San<'a>],
}

pub(crate) struct Ca {
    name: String,
    key: EcdsaKeyPair,
    pub(crate) certificate: Issued,
}

fn generate() -> (EcdsaKeyPair, Vec<u8>) {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let key =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    (key, pkcs8.as_ref().to_vec())
}

fn signed(
    signer: &EcdsaKeyPair,
    issuer: &str,
    spec: &Spec<'_>,
    public_key: &[u8],
    ca: bool,
) -> Vec<u8> {
    let rng = SystemRandom::new();
    let mut serial = [0u8; 16];
    ring::rand::SecureRandom::fill(&rng, &mut serial).unwrap();
    serial[0] &= 0x7F;
    let mut extensions = vec![
        extension(
            &[0x55, 0x1D, 0x13],
            true,
            if ca {
                seq(&[vec![0x01, 0x01, 0xFF]])
            } else {
                seq(&[])
            },
        ),
        // keyCertSign and cRLSign for a CA, digitalSignature for a leaf.
        extension(
            &[0x55, 0x1D, 0x0F],
            true,
            if ca {
                vec![0x03, 0x02, 0x01, 0x06]
            } else {
                vec![0x03, 0x02, 0x07, 0x80]
            },
        ),
    ];
    if !ca {
        extensions.push(extension(
            &[0x55, 0x1D, 0x25],
            false,
            seq(&[tlv(0x06, &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01])]),
        ));
    }
    if !spec.sans.is_empty() {
        let names: Vec<Vec<u8>> = spec
            .sans
            .iter()
            .map(|san| match san {
                San::Dns(dns) => tlv(0x82, dns.as_bytes()),
                San::Ip(ip) => tlv(0x87, ip),
            })
            .collect();
        extensions.push(extension(&[0x55, 0x1D, 0x11], false, seq(&names)));
    }
    let tbs = seq(&[
        tlv(0xA0, &integer(&[2])),
        integer(&serial),
        seq(&[ECDSA_SHA256.to_vec()]),
        name(issuer),
        seq(&[time(spec.from), time(spec.until)]),
        name(spec.common_name),
        seq(&[
            seq(&[EC_PUBLIC_KEY.to_vec(), P256.to_vec()]),
            bit_string(public_key),
        ]),
        tlv(0xA3, &seq(&extensions)),
    ]);
    let signature = signer.sign(&rng, &tbs).unwrap();
    seq(&[
        tbs,
        seq(&[ECDSA_SHA256.to_vec()]),
        bit_string(signature.as_ref()),
    ])
}

impl Ca {
    /// A self-signed CA valid from 2020 to 2120.
    pub(crate) fn new(common_name: &str) -> Self {
        Self::from_spec(&Spec {
            common_name,
            from: 2020,
            until: 2120,
            sans: &[],
        })
    }
    /// A self-signed CA with this validity and these names.
    pub(crate) fn from_spec(spec: &Spec<'_>) -> Self {
        let (key, pkcs8) = generate();
        let der = signed(
            &key,
            spec.common_name,
            spec,
            key.public_key().as_ref(),
            true,
        );
        Self {
            name: spec.common_name.to_owned(),
            key,
            certificate: Issued {
                der,
                key_pkcs8: pkcs8,
            },
        }
    }
    /// A server certificate this CA signs.
    pub(crate) fn issue(&self, spec: &Spec<'_>) -> Issued {
        let (key, pkcs8) = generate();
        Issued {
            der: signed(
                &self.key,
                &self.name,
                spec,
                key.public_key().as_ref(),
                false,
            ),
            key_pkcs8: pkcs8,
        }
    }
    /// A loopback server certificate (127.0.0.1 and localhost), valid now.
    #[cfg(feature = "integration-tests")]
    pub(crate) fn loopback(&self) -> Issued {
        self.issue(&Spec {
            common_name: "omg-test-issuer",
            from: 2020,
            until: 2120,
            sans: &[San::Ip([127, 0, 0, 1]), San::Dns("localhost")],
        })
    }
}

/// A self-signed server certificate (no CA flag), as an endpoint with a
/// single certificate has.
pub(crate) fn self_signed(spec: &Spec<'_>) -> Issued {
    let (key, pkcs8) = generate();
    Issued {
        der: signed(
            &key,
            spec.common_name,
            spec,
            key.public_key().as_ref(),
            false,
        ),
        key_pkcs8: pkcs8,
    }
}

/// A rustls server configuration presenting `leaf`.
#[cfg(feature = "integration-tests")]
pub(crate) fn server(leaf: &Issued) -> std::sync::Arc<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    std::sync::Arc::new(
        rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key_pkcs8.clone())),
        )
        .unwrap(),
    )
}
