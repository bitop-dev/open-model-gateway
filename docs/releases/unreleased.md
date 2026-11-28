# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.20.0](v0.20.0.md). They become the next release's notes.

## Breaking and behaviour changes

- **Video creation needs the file store with video outputs allowed** (#52). A finished video is now stored in the gateway, so `POST /v1/videos` answers **400 `video_storage_unavailable`** until a Platform Admin turns video outputs on (`file_video_enabled`, off by default) on an installation with a file store.
- **Videos completed before this release are no longer served by `/content`** (#52). v0.20.0 passed the server's copy through while the server kept it. Now `/content` serves only the gateway's stored copy, which a video completed before the upgrade never had: it answers **410 `video_output_unavailable`** with `reason: completed_before_storage`. So does a video that a replica of v0.20.0 saw complete during the upgrade, since that replica does not mark it as awaiting its output. A video still running at the upgrade and completed by this release has its output stored.
- **A completed video reads `in_progress` until its output is stored** (#52), so a client that downloads once it reads `completed` finds it. The charge is made when the provider completes it.

## Gateway behavior

- **Finished videos are stored in the gateway** (#52, part 1b). The poller stores each finished video once, encrypted, in the file store:
  - typed `video/mp4`, `video/webm` or `video/quicktime` as received, and any other label as `application/octet-stream`;
  - at most 512 MiB, abandoned after 60 s without data or 25 minutes in all;
  - counted toward the workspace's storage, and carrying the job's key and its holder (object visibility, legal holds, subject exports);
  - kept for `file_video_retention_days` (default 7), then `/content` answers **410 `video_output_expired`**.
- **Downloads** run under the poller's claim, its lease renewed. They take at most half of a replica's poll slots. Failed downloads back off and are retried until 6 hours after the video completed; only `too_large` and a reused provider id are given up at once.
- **`/content`** serves that stored copy with its type, `nosniff` and `Cache-Control: no-store`, always whole (no `Range`).
  - It answers `video_not_ready` while the output is awaited.
  - It answers **503** with the file store unavailable, or when a stored copy's object is missing. That case is logged at error.
  - Thumbnails and spritesheets still come from the server, bounded, a partial answer refused.
- **A completed job is charged whether or not its output could be stored.** When it cannot be, the job records why for good: `output_unavailable: {reason}`, shown in Logs. `/content` then answers **410 `video_output_unavailable`** with that `reason`.
- **A video is created only with storage for it:** the file store accepting video outputs (**400 `video_storage_unavailable`**), and the workspace under its storage quota with 512 MiB counted for each video still awaiting its output (**400 `storage_quota_exceeded`**). Sizing and what to watch: [operations](../operations.md#stored-video-outputs-52).
- **`DELETE` is local first** (#52, part 1b):
  1. The gateway's copy. A database failure there is an error, never a deletion. A file store that fails to remove the object leaves the copy unreadable at once, for the file sweeper.
  2. The record. An output recorded meanwhile is claimed with it, and deleted too.
  3. The server's copy, best effort. A server error no longer fails the delete, and a copy not confirmed gone is logged.
- **`upstream_deleted`** is `true` for `204`, a `200`, a `404` or a create never sent. It is `false` for `405`, `202` (only queued), a refusal or failure, a disabled route, a create of unknown outcome, or a reused id.
- **A deleted video keeps its provider id:** a server that gives it out again has the new job refused as `upstream_id_reused`.
- **Orphaned video outputs are swept:** a stored video output that no job names two hours after its commit is removed by the file sweeper (which looks back one day; one missed goes at its retention). That happens after a crash between storing and recording it, or when its job was deleted or its claim ended first.
- **A download stops when its job is deleted,** at its next chunk: the lease renewal finds the job deleted. Nothing partial is kept.
- **Outputs are stored by the job poller only.** With the poller disabled on every replica, a finished video stays `in_progress`.

## Guardrails

- **What a rule does with content it cannot read (#841, migration 0100).** Unchanged by default: a Block rule keeps `uninspectable: deny`, so a request with an image, audio or a document that isn't plain text is still refused with `guardrail_not_supported` after its text is checked, and existing rules are not rewritten. New: `uninspectable: allow` on a rule lets such a request through unread, the text beside an image still checked and blocked on a match, recorded as `skipped` ("Not readable · let through" in the monthly totals, "Let through unread" on the request page). A native batch is still refused by a Block rule under either value: its lines are text a gateway-run batch reads. New: a rule may name the `images`, `audio_transcriptions` and `realtime` workloads, which this release never reads; a Block rule naming them refuses every request of that workload with `guardrail_not_supported` before admission (a realtime session before the WebSocket upgrade), and with `allow` they pass unread and record `skipped`; a realtime session's decisions are recorded with operation `realtime`. Rules that name only generation and embeddings, every rule written before, change nothing for these workloads. `allow` is the weaker value: a workspace's tightening may move a platform rule from `allow` to `deny`, never back (`403 guardrail_loosen_not_allowed`), and composition takes `deny`. Every decision now records its rule's own verdict (`refuses`), and the monthly totals split not readable decisions by it (`refuses`, null on other rows). The rule dialog says before saving when a Block rule would refuse every request of a workload for every workspace its layer reaches. [Content a rule cannot read](../guardrails-design.md#content-a-rule-cannot-read-841)

## Upgrade

- **#52:**
  - **Drain the gateway before upgrading.** A replica of v0.20.0 that sees a video complete does not mark it as awaiting its output, so that video reads `completed_before_storage` for good.
  - Reapply the runtime grants: `UPDATE(output_pending, output_failure)` on `async_jobs`.
  - Turn on video outputs if videos are to be created (see above).
  - Run migration 0101 (below).

### Migrations

**Migration 0100** (#841) replaces the 0092 CHECK on `guardrail_rules` that every tightening entry meets, found by its definition (exactly one must match, or the migration stops with a hint and applies nothing), with `guardrail_rules_tightening`: a tightening may now set `uninspectable`, only to `deny`, and that alone is enough for a tightening. It is validated in place, a scan of the small rules table; no stored row can violate it, because every row met the CHECK it replaces, which required `uninspectable` to be null on a tightening. It replaces the operation CHECK of `guardrail_events` the same way with `guardrail_events_operation`, which adds `realtime`, added **unvalidated** (`NOT VALID`) on the partitioned table and its month partitions (partitions created later carry it too): the list only grows, so every earlier row meets it, and validating would scan the decision history under the lock. It adds `guardrail_events.refuses` (`boolean`, nullable, no default: no rewrite), each decision's rule's own verdict, null on every earlier row, with the CHECK `guardrail_events_refuses` (every new row states it, and the decision the client was refused with refuses), unvalidated like the operation CHECK. **Apply `deploy/staging/runtime-grants.sql` before the new binary starts** (`grants apply`): it grants `INSERT` on `refuses`, which the new binary names in every decision insert, so without it every guardrail decision is refused, also for rules that existed before. **Drain the gateway before migrating** (scale it to 0; [upgrade procedure](../operations.md#upgrade-and-migration-procedure)): the migration is one transaction holding `ACCESS EXCLUSIVE` on `guardrail_rules` and on `guardrail_events` and its partitions until it commits, with `lock_timeout` 10 s (on a timeout it rolls back and can be rerun). **A replica of the previous release must not serve once 0100 has committed** (the drain above): its decision insert names no `refuses`, so the new CHECK refuses **every** decision it records, whatever the rule. Each request it would record a decision for then fails with `503 accounting_unavailable`: a Monitor finding fails the request instead of letting it on, and a blocked request answers 503 instead of 400. It also ignores a tightening's `uninspectable: deny` and never guards a realtime session. It does not stay in service on its own for long: v0.20.0's readiness requires its own exact migration lineage, so it reports not ready once 0100 is applied. **Rollback to v0.20.0** is the restore of the pre-upgrade backup, as for every release with migrations (there are no down migrations): v0.20.0 does not become ready on a database at 0100, so dropping `guardrail_events_refuses` would not let it serve. The restore loses the decisions recorded since the upgrade and every rule written since; a rule with `uninspectable: allow` or naming `images`, `audio_transcriptions` or `realtime` must not be written again on v0.20.0, which refuses it on write (`guardrail_detector_unavailable`), and would treat one it found stored as a rule that needs a later release (failing under its `on_error`).

**Migration 0101** (#52) adds the partial index `async_jobs_video_upstream_id` on `async_jobs(upstream_id) WHERE kind = 'video'`.
- The check that one upstream video id belongs to one job uses it on every video create. Before, that check scanned every video job.
- It changes no row and adds no constraint and no grant.
- Building it holds a SHARE lock on `async_jobs` (reads go on, writes wait) for one scan of the table. Drain the gateway first, as for any migration ([upgrade procedure](../operations.md#upgrade-and-migration-procedure)).
- `lock_timeout` is 10 s, for this migration only; on a timeout it rolls back and can be rerun.
