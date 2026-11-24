# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Gateway behavior

- The job poller now claims each video, native batch and 0016 batch with an owner, a renewable 60 s lease and a claim epoch on the job row, as the batch runner does for gateway-run batches (#651). Two gateway processes no longer poll the same job at once, and a poller that lost a native batch to another (it stalled past its lease) before it recorded the batch's results writes nothing more to it: it records no status or counts, does not finish or settle it, and deletes the result files it wrote, so a batch no longer gets duplicate, unlinked result files. A poller whose lease lapses after it recorded the results still settles the batch (settlement applies once) and deletes the provider's copies. A submission that fails after its poller lost the claim now ends `submission_interrupted` with its hold kept as unknown, where it could take the free release before. Video and 0016 batch observations are claimed but not fenced: only their state change is compare-and-set, so a stale observation can still rewrite progress, counts and display status with older values. A crashed process's jobs are polled again after about a minute. [Poller](../async-jobs.md#poller)

## Upgrade

No migration or grant change: the claim uses the `runner_id`, `runner_lease_until` and `runner_epoch` columns and grants of 0021 and 0081. The claim protects a job only once every gateway process runs this release. A process of an earlier release still polls with the old 30 s soft claim and ignores the lease, so drain older replicas rather than running them beside this one.

## Metrics

- `gateway_upstream_connections_opened_total{client}` counts the connections each pooled outbound client opens, with the same fixed `client` label as `gateway_upstream_connection_lost_total` (#611). It gives the `s3` and `export` clients a denominator for their loss counter and shows what each pool idle timeout costs. Both counters now export every client's series from startup at zero (`gateway_upstream_connection_lost_total` used to appear only after the first loss), and the example rules add `GatewayUpstreamConnectionLoss`, computed per replica. The idle timeouts stay fixed constants: a per-endpoint setting for local endpoints and tuned cloud values wait for what the two counters show in real deployments. [Lost connections](../operations.md#lost-connections-483)
