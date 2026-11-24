# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Gateway behavior

- The job poller now claims each video, native batch and 0016 batch with an owner, a renewable 60 s lease and a claim epoch on the job row, as the batch runner does for gateway-run batches (#651). Two gateway processes no longer poll the same job at once, and a poller that lost a native batch to another (it stalled past its lease) before it recorded the batch's results writes nothing more to it: it records no status or counts, does not finish or settle it, and deletes the result files it wrote, so a batch no longer gets duplicate, unlinked result files. A poller whose lease lapses after it recorded the results still settles the batch (settlement applies once) and deletes the provider's copies. A submission that fails after its poller lost the claim now ends `submission_interrupted` with its hold kept as unknown, where it could take the free release before. Video and 0016 batch observations are claimed but not fenced: only their state change is compare-and-set, so a stale observation can still rewrite progress, counts and display status with older values. A crashed process's jobs are polled again after about a minute. [Poller](../async-jobs.md#poller)

## Upgrade

No migration or grant change: the claim uses the `runner_id`, `runner_lease_until` and `runner_epoch` columns and grants of 0021 and 0081. The claim protects a job only once every gateway process runs this release. A process of an earlier release still polls with the old 30 s soft claim and ignores the lease, so drain older replicas rather than running them beside this one.

## Internal

- **Provider-specific admission and scheduling behaviour is now declared by the adapters** (#624). Whether Messages `cache_control` hints are priced at admission and in batch-line planning follows `ProviderAdapter::forwards_cache_controls` (only Anthropic), and whether a route's batch scheduling may set a priority hint follows the local profile's `accepts_priority_hint` (`vllm`, `openai_compatible`), instead of provider-name comparisons in `governance` and `jobs`. The provider-switch source check now covers `governance`, `routing` and `jobs/` with no exceptions, pins the files outside that scope that name a provider, and pins the shared submodules the scope may use. No behaviour change: the same requests admit with the same holds and prices, and the same routes accept priority hints. A new adapter that forwards cache hints sets `ProviderAdapter::forwards_cache_controls`; a new local profile whose servers accept the `priority` field sets `local::Profile::accepts_priority_hint`. An adapter that is not a local profile cannot declare priority hints today (`capabilities::accepts_priority_hint` answers `false` for it).
