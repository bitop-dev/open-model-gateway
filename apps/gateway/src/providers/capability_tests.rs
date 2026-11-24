//! The provider-specific behaviour that admission and batch scheduling read
//! without naming a provider, checked for every adapter `serve` registers
//! (`providers::served_adapters`, the list `serve` registers): a capability
//! wired to the wrong adapter, or a new adapter without its row, fails here.
use std::sync::Arc;

use super::{
    ProviderAdapter, capabilities, local::endpoints::ApprovedEndpoints,
    openrouter::OpenRouterConfig, secrets::EnvSecrets,
};

/// The adapters `commands::serve` registers, in its order. Building them
/// makes no request.
fn served() -> Vec<Arc<dyn ProviderAdapter>> {
    super::served_adapters(
        Arc::new(EnvSecrets::new([])),
        OpenRouterConfig::default(),
        ApprovedEndpoints::for_test("http://127.0.0.1:9/v1"),
    )
    .unwrap()
}

#[test]
fn every_served_adapter_declares_its_capabilities() {
    let declared: Vec<(&str, bool, bool)> = served()
        .iter()
        .map(|a| {
            (
                a.id(),
                a.forwards_cache_controls(),
                capabilities::accepts_priority_hint(a.id()),
            )
        })
        .collect();
    // (id, forwards Messages cache_control hints, accepts a priority hint)
    assert_eq!(
        declared,
        [
            ("openai", false, false),
            ("anthropic", true, false),
            ("bedrock", false, false),
            ("openrouter", false, false),
            ("openai_compatible", false, true),
            ("vllm", false, true),
            ("sglang", false, false),
            ("ollama", false, false),
        ]
    );
    // Every registered connection profile is a served adapter.
    let mut ids: Vec<&str> = declared.iter().map(|d| d.0).collect();
    let mut profiles: Vec<&str> = capabilities::PROFILES.iter().map(|p| p.0).collect();
    ids.sort_unstable();
    profiles.sort_unstable();
    assert_eq!(ids, profiles);
}

#[test]
fn an_unknown_profile_accepts_no_priority_hint() {
    for profile in ["", "VLLM", "vllm ", "azure", "openai_compatible2"] {
        assert!(!capabilities::accepts_priority_hint(profile), "{profile:?}");
    }
}
