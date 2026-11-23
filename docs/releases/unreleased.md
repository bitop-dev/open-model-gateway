# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Gateway behavior

- The job poller now claims each video, native batch and 0016 batch with an owner, a renewable 60 s lease and a claim epoch on the job row, as the batch runner does for gateway-run batches (#651). Two gateway processes no longer poll the same job at once, and a poller that lost a native batch to another (it stalled past its lease) writes nothing more to it: it records no status or counts, does not finish or settle it, and deletes the result files it wrote, so a batch no longer gets duplicate, unlinked result files. A crashed process's jobs are polled again after about a minute. [Poller](../async-jobs.md#poller)

## Upgrade

No migration or grant change: the claim uses the `runner_id`, `runner_lease_until` and `runner_epoch` columns and grants of 0021 and 0081. The claim protects a job only once every gateway process runs this release. A process of an earlier release still polls with the old 30 s soft claim and ignores the lease, so drain older replicas rather than running them beside this one.
