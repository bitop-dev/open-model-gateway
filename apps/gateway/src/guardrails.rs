//! Guardrails (docs/guardrails-design.md): rules that inspect request text
//! before it reaches a provider.
//!
//! Phase 1 (#840): rule storage in three layers, tighten-only composition and
//! the management API; the pattern detector (built-in secret shapes and
//! custom patterns); and input enforcement in the engine for generation and
//! embeddings, with monitor and block, recorded in `guardrail_events`. Output
//! is not inspected yet.
pub mod detector;
pub mod enforce;
pub mod limits;
pub mod patterns;
pub mod policy;
pub mod secrets;
pub mod shared;
pub mod stored;
pub mod view;
pub mod write;
