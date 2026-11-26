//! What the process log never carries, whatever `RUST_LOG` says.
//!
//! The AWS SDK writes a whole HTTP request, body included, as a TRACE event:
//! `aws_smithy_runtime` "transmitting request", and `aws_sigv4` "signing
//! request" when `LOG_SIGNABLE_BODY` is set. A Bedrock request body holds the
//! prompt and every inline image (#24), so an operator's `RUST_LOG=trace`
//! must not put it in the log: TRACE from the `aws_*` crates is never
//! enabled. Their DEBUG and above follow `RUST_LOG` as before.
//! `main` installs [`allowed`] beside the `RUST_LOG` filter, and
//! `providers::bedrock` tests a real request through both.

/// Whether an event or span of `metadata` may be recorded.
pub fn allowed(metadata: &tracing::Metadata<'_>) -> bool {
    !(*metadata.level() == tracing::Level::TRACE && metadata.target().starts_with("aws_"))
}
