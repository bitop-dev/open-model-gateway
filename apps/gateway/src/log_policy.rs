//! What the process log never carries, whatever `RUST_LOG` says.
//!
//! The AWS SDK writes each HTTP request it sends as a TRACE event
//! (`aws_smithy_runtime` "transmitting request"), and `aws_sigv4` writes the
//! request it signs as another ("signing request"). A Bedrock request body
//! holds the prompt and every inline image (#24). The SDK as locked prints
//! neither body: the transmitted request's body is boxed and shown only as
//! `BoxBody`, and `aws_sigv4` redacts the signed body unless
//! `LOG_SIGNABLE_BODY=true` is set in the environment. That switch, and an
//! SDK version that prints more, are what this policy covers: TRACE from the
//! `aws_*` crates is never recorded, so an operator's `RUST_LOG=trace` cannot
//! put a request body in the log. Their DEBUG and above follow `RUST_LOG`.
//!
//! [`subscriber`] is the process subscriber that `main::run` installs for
//! every command it runs (all but `healthcheck`, `schema-version` and
//! `archive verify`, which return before it and use no AWS SDK);
//! `providers::bedrock` tests it on a real request.
use tracing_subscriber::{EnvFilter, fmt::MakeWriter, layer::SubscriberExt as _};

/// Whether an event or span of `metadata` may be recorded.
pub fn allowed(metadata: &tracing::Metadata<'_>) -> bool {
    !(*metadata.level() == tracing::Level::TRACE && metadata.target().starts_with("aws_"))
}

/// JSON log lines to `writer`, recording what `filter` (the `RUST_LOG`
/// filter) enables and [`allowed`] permits: the policy sits beside the
/// filter, so no filter string lifts it.
pub fn subscriber<W>(writer: W, filter: EnvFilter) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_env_filter(filter)
        .finish()
        .with(tracing_subscriber::filter::filter_fn(allowed))
}
