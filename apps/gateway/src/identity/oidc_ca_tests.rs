//! #826: `GATEWAY_OIDC_CA_FILE` on the real path. An HTTPS issuer whose
//! certificate a generated private CA signs is reached (discovery, JWKS and
//! the token exchange, the only requests the OIDC client makes) with those
//! roots and refused without them; the roots never make a wrong certificate
//! acceptable (another CA, expired, another name); and no other client
//! trusts them.
use super::*;
use crate::{
    egress::CaBundle,
    identity::test_pki::{Ca, San, Spec, server},
};

fn config(mock: &MockProvider, ca: Option<&str>) -> IdentityConfig {
    let mut config = mock.config();
    config.ca =
        ca.map(|pem| Arc::new(CaBundle::parse("GATEWAY_OIDC_CA_FILE", pem.as_bytes()).unwrap()));
    config
}
async fn connect(
    pool: &sqlx::PgPool,
    mock: &MockProvider,
    ca: Option<&str>,
) -> anyhow::Result<IdentityState> {
    IdentityState::new(Store::new(pool.clone()), Some(config(mock, ca))).await
}

/// Discovery and JWKS at startup, and the token exchange of a sign-in, over
/// TLS to a private-CA issuer: only with its roots.
#[dbtest::test]
async fn a_private_ca_issuer_is_reached_only_with_its_roots(pool: sqlx::PgPool) {
    let ca = Ca::new("OMG test CA");
    let mock = MockProvider::start_tls(server(&ca.loopback())).await;
    assert!(mock.issuer.starts_with("https://127.0.0.1:"));
    connect(&pool, &mock, None)
        .await
        .err()
        .expect("public roots only");
    assert_eq!(
        mock.jwks_hits.load(Ordering::SeqCst),
        0,
        "discovery failed first"
    );
    let identity = connect(&pool, &mock, Some(&ca.certificate.pem()))
        .await
        .unwrap();
    assert!(
        mock.jwks_hits.load(Ordering::SeqCst) >= 1,
        "JWKS over TLS at startup"
    );
    // A sign-in: the authorization redirect, then the token exchange over TLS.
    let engine = crate::inference::Engine::new(
        Arc::new(identity.store.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(identity.store.clone(), None, engine, identity);
    let r = open_get(&app, "/api/v1/auth/login", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let url = Url::parse(r.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert!(url.as_str().starts_with(&mock.issuer));
    let params: HashMap<_, _> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    mock.claims.lock().unwrap()["nonce"] = json!(params["nonce"]);
    let browser = captured_cookie(&r.headers, BROWSER);
    let path = format!("/api/v1/auth/callback?state={}&code=good", params["state"]);
    // The outcome (this test user has no platform access) is not the point:
    // the code was exchanged with the issuer, over TLS, with its roots.
    let request = Request::builder()
        .uri(&path)
        .header("cookie", format!("{BROWSER}={browser}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        mock.exchanges.load(Ordering::SeqCst),
        1,
        "the token exchange reached the issuer"
    );
    assert!(
        mock.verifier.lock().unwrap().is_some(),
        "with its PKCE verifier"
    );
}

/// The extra roots add trust in one CA's certificates for the issuer's name,
/// nothing else: a certificate from another private CA, an expired one and
/// one for another name are refused with the setting. A bundle of several
/// roots trusts each.
#[dbtest::test]
async fn the_roots_never_accept_a_wrong_certificate(pool: sqlx::PgPool) {
    let ca = Ca::new("OMG test CA");
    let other = Ca::new("OMG other CA");
    // The same name with another key: refused by the signature check alone.
    let impostor = Ca::new("OMG test CA");
    let loopback = [San::Ip([127, 0, 0, 1]), San::Dns("localhost")];
    let expired = ca.issue(&Spec {
        common_name: "omg-expired",
        from: 2000,
        until: 2010,
        sans: &loopback,
    });
    let not_yet = ca.issue(&Spec {
        common_name: "omg-future",
        from: 2100,
        until: 2110,
        sans: &loopback,
    });
    let wrong_name = ca.issue(&Spec {
        common_name: "issuer.example.test",
        from: 2020,
        until: 2120,
        sans: &[San::Dns("issuer.example.test"), San::Ip([127, 0, 0, 2])],
    });
    // A CA certificate is not a server certificate, even a trusted CA's
    // carrying the issuer's address: refused for its CA flag, not its name.
    let named_ca = Ca::from_spec(&Spec {
        common_name: "OMG named CA",
        from: 2020,
        until: 2120,
        sans: &loopback,
    });
    let ca_as_leaf = crate::identity::test_pki::Issued {
        der: named_ca.certificate.der.clone(),
        key_pkcs8: named_ca.certificate.key_pkcs8.clone(),
    };
    for (what, leaf, roots) in [
        ("another CA", other.loopback(), ca.certificate.pem()),
        (
            "another key under the trusted CA's name",
            impostor.loopback(),
            ca.certificate.pem(),
        ),
        ("expired", expired, ca.certificate.pem()),
        ("not yet valid", not_yet, ca.certificate.pem()),
        ("another name", wrong_name, ca.certificate.pem()),
        (
            "a CA certificate as the server's",
            ca_as_leaf,
            named_ca.certificate.pem(),
        ),
    ] {
        let mock = MockProvider::start_tls(server(&leaf)).await;
        assert!(
            connect(&pool, &mock, Some(&roots)).await.is_err(),
            "{what} must be refused"
        );
        assert_eq!(mock.jwks_hits.load(Ordering::SeqCst), 0, "{what}");
    }
    let mock = MockProvider::start_tls(server(&ca.loopback())).await;
    let bundle = format!("{}{}", other.certificate.pem(), ca.certificate.pem());
    connect(&pool, &mock, Some(&bundle)).await.unwrap();
    // The other CA alone does not vouch for this CA's issuer.
    assert!(
        connect(&pool, &mock, Some(&other.certificate.pem()))
            .await
            .is_err()
    );
}

/// The roots belong to the OIDC client alone: while sign-in to the
/// private-CA issuer works, the export client (alert webhooks, traces,
/// metrics, audit export) built with the same loopback permission refuses
/// the very same certificate.
#[dbtest::test]
async fn no_other_client_trusts_the_roots(pool: sqlx::PgPool) {
    let ca = Ca::new("OMG test CA");
    let mock = MockProvider::start_tls(server(&ca.loopback())).await;
    connect(&pool, &mock, Some(&ca.certificate.pem()))
        .await
        .unwrap();
    let url = Url::parse(&format!("{}/.well-known/openid-configuration", mock.issuer)).unwrap();
    let export = crate::otlp::export_http_client_with_policy(
        &url,
        crate::egress::Purpose::AlertWebhook,
        crate::egress::policy().permit_loopback(true),
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let error = export.get(url.clone()).send().await.unwrap_err();
    // Refused for the certificate (its issuer is unknown to that client),
    // not for any reason before the handshake.
    let chain = format!("{:?}", anyhow::Error::from(error));
    assert!(chain.contains("UnknownIssuer"), "{chain}");
    // The OIDC client of this provider still reaches it.
    connect(&pool, &mock, Some(&ca.certificate.pem()))
        .await
        .unwrap();
}
