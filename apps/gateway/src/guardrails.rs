//! Guardrails (docs/guardrails-design.md): rules that inspect request text
//! before it reaches a provider.
//!
//! This release (#840, phase 1, first part) adds the policy model only: rule
//! storage in three layers, tighten-only composition and the management API.
//! **Nothing inspects request text yet**: no request is monitored or blocked
//! until the detector and the engine step land.
pub mod patterns;
pub mod policy;
