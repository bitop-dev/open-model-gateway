# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Internal

- **Provider-specific admission and scheduling behaviour is now declared by the adapters** (#624). Whether Messages `cache_control` hints are priced at admission and in batch-line planning follows `ProviderAdapter::forwards_cache_controls` (only Anthropic), and whether a route's batch scheduling may set a priority hint follows the local profile's `accepts_priority_hint` (`vllm`, `openai_compatible`), instead of provider-name comparisons in `governance` and `jobs`. The provider-switch source check now covers `governance`, `routing` and `jobs/` with no exceptions, pins the files outside that scope that name a provider, and pins the shared submodules the scope may use. No behaviour change: the same requests admit with the same holds and prices, and the same routes accept priority hints. A new adapter that forwards cache hints sets `ProviderAdapter::forwards_cache_controls`; a new local profile whose servers accept the `priority` field sets `local::Profile::accepts_priority_hint`. An adapter that is not a local profile cannot declare priority hints today (`capabilities::accepts_priority_hint` answers `false` for it).
