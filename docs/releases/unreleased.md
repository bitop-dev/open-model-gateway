# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Gateway behavior

- The job poller now claims each video, native batch and 0016 batch with an owner, a renewable 60 s lease and a claim epoch on the job row, as the batch runner does for gateway-run batches (#651). Two gateway processes no longer poll the same job at once, and a poller that lost a native batch to another (it stalled past its lease) before it recorded the batch's results writes nothing more to it: it records no status or counts, does not finish or settle it, and deletes the result files it wrote, so a batch no longer gets duplicate, unlinked result files. A poller whose lease lapses after it recorded the results still settles the batch (settlement applies once) and deletes the provider's copies. A submission that fails after its poller lost the claim now ends `submission_interrupted` with its hold kept as unknown, where it could take the free release before. Video and 0016 batch observations are claimed but not fenced: only their state change is compare-and-set, so a stale observation can still rewrite progress, counts and display status with older values. A crashed process's jobs are polled again after about a minute. [Poller](../async-jobs.md#poller)

## Upgrade

No migration or grant change: the claim uses the `runner_id`, `runner_lease_until` and `runner_epoch` columns and grants of 0021 and 0081. The claim protects a job only once every gateway process runs this release. A process of an earlier release still polls with the old 30 s soft claim and ignores the lease, so drain older replicas rather than running them beside this one.

## Development bootstrap

`bootstrap-dev` (and `bootstrap-demo`, which shares the check) now refuses any installation already in use: a row in any table either seed writes (accounts, grants, group mappings, cost centers, workspaces, service accounts, keys, catalog entries, budgets, audit events), execution history, or a configuration topic that has moved since migration (#743). A configuration-only database is no longer seeded with a platform admin. A repeat run on an installation with its own bootstrap marker is unchanged. The local demo's database check (for `bootstrap-demo` and for `serve` with the demo issuer) now also refuses a Unix socket (`?host=/...`), as `bootstrap-dev` already did. The refusal for a non-literal loopback host (for example `localhost`) now says to write `127.0.0.1` or `[::1]` in `DATABASE_URL`. A concurrent first OIDC sign-in is serialized with the seed by the catalog and installation locks both already take; tests now cover both orders. State only in other tables, and a loopback forwarder to a freshly migrated remote database, still pass the guard. [Local demo](../local-demo.md)
