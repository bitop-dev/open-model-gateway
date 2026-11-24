//! Whether an attempt's adapter call can have written anything upstream.
//!
//! Every outbound HTTP request of a provider adapter is built through
//! `crate::egress::HttpClient` (Clippy's `disallowed-types` and the client
//! module scan refuse any other reqwest client, and Clippy refuses
//! `RequestBuilder::try_clone`, which would copy a builder without building
//! it), and each of its request constructors calls [`started`] before it
//! returns a builder. The realtime WebSocket, the one provider connection
//! that does not use it, calls [`started`] before it connects. The engine
//! runs each attempt's adapter call inside [`observe`]; if that call ended
//! without [`started`] having run, no request to the provider was even built,
//! so not one byte of it can have been written. That proof is structural (it
//! does not depend on which error the adapter returned), and it is the only
//! thing that lets settlement record a failed attempt as a known zero on a
//! priced route (`governance::settlement`, `nothing_sent`).
//!
//! The mark is set when a request is *built*, which is before anything is
//! written, so a failure between building and sending (an invalid header,
//! say) is conservatively treated as possibly sent. The scope is a task-local,
//! so it holds only because adapters run their upstream calls in the polling
//! task and never spawn them (the `ProviderAdapter` contract: no detached
//! producers; the AWS SDK's orchestrator calls its connector inline). Outside
//! a scope (jobs, exporters, tests) [`started`] does nothing.
use std::{cell::Cell, future::Future};

tokio::task_local! {
    static STARTED: Cell<bool>;
}

/// Run one attempt's adapter call, returning its output and whether it
/// ended with nothing sent: `true` only if no upstream request was built
/// while it ran, including when it was dropped by a timeout around it.
pub(crate) async fn observe<F: Future>(future: F) -> (F::Output, bool) {
    STARTED
        .scope(Cell::new(false), async move {
            let output = future.await;
            (output, !STARTED.with(Cell::get))
        })
        .await
}

/// Record that an upstream request is being built. Called by every request
/// constructor of `crate::egress::HttpClient`; a no-op outside [`observe`].
pub(crate) fn started() {
    let _ = STARTED.try_with(|cell| cell.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_a_scope_without_a_built_request_proves_nothing_sent() {
        let (value, nothing_sent) = observe(async { 7 }).await;
        assert_eq!((value, nothing_sent), (7, true));
        let (_, nothing_sent) = observe(async { started() }).await;
        assert!(!nothing_sent);
        // A request built before a dropped inner future still counts.
        let (timed_out, nothing_sent) = observe(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            async {
                started();
                std::future::pending::<()>().await
            },
        ))
        .await;
        assert!(timed_out.is_err());
        assert!(!nothing_sent);
        // Each scope starts unmarked; a mark outside any scope is harmless.
        started();
        assert!(observe(async {}).await.1);
    }

    #[tokio::test]
    async fn the_client_marks_every_request_it_builds() {
        let client = crate::egress::HttpClient::from_test_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );
        let url = "http://127.0.0.1:9/";
        assert!(!observe(async { client.get(url) }).await.1);
        assert!(!observe(async { client.post(url) }).await.1);
        assert!(!observe(async { client.delete(url) }).await.1);
        assert!(
            !observe(async { client.request(reqwest::Method::PUT, url) })
                .await
                .1
        );
        let request = reqwest::Request::new(reqwest::Method::GET, url.parse().unwrap());
        // `execute` marks before it sends; the send itself fails (port 9).
        assert!(!observe(client.execute(request)).await.1);
    }
}
