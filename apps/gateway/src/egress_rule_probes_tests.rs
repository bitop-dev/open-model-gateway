//! Each function calls one connection constructor that `clippy.toml` refuses
//! (#606), under its own `#[expect(clippy::disallowed_methods)]`. If an entry
//! stops matching (a renamed path, a moved crate, a removed line), its
//! expectation is unfulfilled and Clippy fails (`unfulfilled_lint_expectations`
//! under `-D warnings`). Nothing here runs: the functions are never called.
//! The three production statements that open sockets prove the TCP entries
//! they use the same way, in the build without `test`.
//!
//! `dead_code` is allowed for the whole module because the probes are never
//! called; the scan permits no other lint switch here (only these `expect`s).
#![allow(dead_code)]
use std::{net::SocketAddr, time::Duration};

use hyper_util::rt::{TokioExecutor, TokioIo};

type Io = TokioIo<tokio::io::DuplexStream>;

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_http1_handshake(io: Io) {
    drop(hyper::client::conn::http1::handshake::<_, String>(io));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_http1_builder() {
    let _ = hyper::client::conn::http1::Builder::new();
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_http1_builder_handshake(builder: &hyper::client::conn::http1::Builder, io: Io) {
    drop(builder.handshake::<_, String>(io));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_http2_handshake(io: Io) {
    drop(hyper::client::conn::http2::handshake::<_, _, String>(
        TokioExecutor::new(),
        io,
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_http2_builder() {
    drop(hyper::client::conn::http2::Builder::new(
        TokioExecutor::new(),
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_http2_builder_handshake(
    builder: &hyper::client::conn::http2::Builder<TokioExecutor>,
    io: Io,
) {
    drop(builder.handshake::<_, String>(io));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_util_client_builder() {
    drop(hyper_util::client::legacy::Client::builder(
        TokioExecutor::new(),
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_util_builder_new() {
    drop(hyper_util::client::legacy::Builder::new(
        TokioExecutor::new(),
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_util_builder_build(
    builder: &hyper_util::client::legacy::Builder,
    connector: hyper_util::client::legacy::connect::HttpConnector,
) {
    drop(builder.build::<_, String>(connector));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_util_builder_build_http(builder: &hyper_util::client::legacy::Builder) {
    drop(builder.build_http::<String>());
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_util_http_connector() {
    drop(hyper_util::client::legacy::connect::HttpConnector::new());
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_util_http_connector_with_resolver() {
    drop(
        hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(
            hyper_util::client::legacy::connect::dns::GaiResolver::new(),
        ),
    );
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_rustls_connector() {
    let _ = hyper_rustls::HttpsConnectorBuilder::new();
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_rustls_build(
    builder: hyper_rustls::HttpsConnectorBuilder<hyper_rustls::builderstates::WantsProtocols2>,
) {
    drop(builder.build());
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_rustls_wrap_connector(
    builder: hyper_rustls::HttpsConnectorBuilder<hyper_rustls::builderstates::WantsProtocols2>,
    connector: hyper_util::client::legacy::connect::HttpConnector,
) {
    drop(builder.wrap_connector(connector));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn hyper_rustls_connector_builder() {
    let _ =
        hyper_rustls::HttpsConnector::<hyper_util::client::legacy::connect::HttpConnector>::builder(
        );
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tungstenite_connect() {
    drop(tokio_tungstenite::tungstenite::client::connect(
        "ws://127.0.0.1:1",
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tungstenite_connect_with_config() {
    drop(tokio_tungstenite::tungstenite::client::connect_with_config(
        "ws://127.0.0.1:1",
        None,
        1,
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tungstenite_connect_async() {
    drop(tokio_tungstenite::connect_async("ws://127.0.0.1:1"));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tungstenite_connect_async_with_config() {
    drop(tokio_tungstenite::connect_async_with_config(
        "ws://127.0.0.1:1",
        None,
        false,
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tungstenite_connect_async_tls_with_config() {
    drop(tokio_tungstenite::connect_async_tls_with_config(
        "ws://127.0.0.1:1",
        None,
        false,
        None,
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tokio_tcp_stream_connect(address: SocketAddr) {
    drop(tokio::net::TcpStream::connect(address));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tokio_tcp_socket_connect(socket: tokio::net::TcpSocket, address: SocketAddr) {
    drop(socket.connect(address));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn tokio_udp_socket_bind(address: SocketAddr) {
    drop(tokio::net::UdpSocket::bind(address));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn std_tcp_stream_connect(address: SocketAddr) {
    drop(std::net::TcpStream::connect(address));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn std_tcp_stream_connect_timeout(address: SocketAddr) {
    drop(std::net::TcpStream::connect_timeout(
        &address,
        Duration::from_secs(1),
    ));
}

#[expect(
    clippy::disallowed_methods,
    reason = "probe of a refused constructor (#606)"
)]
fn std_udp_socket_bind(address: SocketAddr) {
    drop(std::net::UdpSocket::bind(address));
}
