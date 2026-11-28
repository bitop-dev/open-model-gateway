# Async jobs: video generation and batches

The gateway proxies two kinds of long-running provider job:

- **Video generation:** `POST /v1/videos`, on any server of the OpenAI video contract behind an approved OpenAI-compatible connection (see below).
- **Batches:** `POST /v1/batches` from a gateway file (`purpose=batch`, [Files API](files-api.md)), for any model. They run natively on a provider batch API or line by line through the gateway. See [batches](batches.md); this page keeps what batches share with video jobs.

A video job and a native batch are **one upstream attempt**, with one execution and one durable reservation, admitted when created and settled when the provider reports a terminal state. A gateway-run batch's lines are separate attempts with their own reservations ([batches](batches.md#pricing-and-budgets)). Migrations `0016_async_jobs.sql` and `0021_batch_engine.sql` store metadata only: never prompts, batch lines, `metadata` values or provider messages. Batch results are stored, encrypted, in the gateway's [file store](files-api.md), never in the database.

This page describes the current source. It is mock-tested (video: a mock server of the OpenAI video contract behind a running gateway and poller). No live video request has been made. Batches had one small capped live check (see [batches](batches.md)).

> **Video: the OpenAI video contract on OpenAI-compatible connections (#52).** OpenAI shut down the Sora 2 models and its own Videos API on 2026-09-24, so OpenAI connections no longer offer `videos`; jobs created before then remain readable from gateway records. Any other server of the same contract (`POST /videos`, `GET /videos/{id}`, `GET /videos/{id}/content`, `DELETE /videos/{id}`) is served through an approved `openai_compatible` connection ([providers](provider-adapters.md)):
>
> - **Each route states what it accepts** (its video options, set by a Platform Admin on the route: migration 0097): the clip lengths, the first of them used when a request names none, and the sizes (none listed: a request may not name a size). A route without options serves no video, and a video model none of whose routes has them reads Not serving. Requests are matched against the caller's own routes of the model (enabled, offered to its workspace and allowed to its key): a length or size none of them takes is refused before admission with **400 `unsupported_video_parameter`** and `param` `seconds` or `size`, and a model none of whose routes has options answers **400 `unsupported_capability`**. The options are read again under the catalog lock when the request is admitted. Nothing is sent and no job or hold is created.
> - **Other options are refused by name:** a field other than `model`, `prompt`, `seconds` and `size` (OpenAI's `input_reference`, or `quality`, `seed`, `audio`, `aspect_ratio` and the like) is refused with **400 `unsupported_video_parameter`**, `param` naming it; nothing is dropped. A field name that is not a plain identifier (up to 64 letters, digits and underscores) cannot be named in `param` and is refused as an invalid request (`invalid_request_error`, no `param`). A length and a size that each some route takes, but no route takes together, are refused with `unsupported_video_parameter` and no `param`.
> - **A video route must price video seconds:** the route's latest price must be pricing v3 with an `output_video_seconds_ms` rate for every size the request may produce, as settlement values it: a size the request names by its own lines, or else the line without a size; with no size named, the price must have one rate for every size (no size-specific line) or name each size the route lists, and a route listing no sizes needs that one rate. A line with a prompt-token tier values nothing, since a video reports no prompt tokens. Only routes priced this way are chosen; when none of the routes that take the request is, it is refused before admission with **400 `video_price_missing`**, budgeted or not ("missing rates are not zero").
> - **What is sent:** the length is always sent, so the billed duration is known; `size` only when the request named one (the job then records it; otherwise its `size` reads `null`). The job's `seconds` is the admitted length (the one named, or the route's first), not the server's report; its `progress` is the server's when it reports one, and otherwise the gateway's own reading (0 until completed, then 100).
> - **Settlement** is from the duration the server reports (its `usage.output_video_seconds` when present, and then only that: a value that is not a whole number of seconds from 1 to 3600 is an unknown duration; else the video's `seconds`), priced per second at the size billed: the size the request named; or, when it named none, the size the server reports, only if the job's price (fixed at admission) names that size itself or has one rate for every size. A reported size other than the named one, one the price neither names nor rates flat, one the gateway cannot read, or none at all for a request that named none (unless the price has one rate for every size) keeps the hold with the cost unknown, as a longer clip than requested and an unknown duration do (whatever the rates: a free video line never settles a video of unknown length). A shorter clip is billed as reported.
> - **One upstream video belongs to one job.** The server must issue video ids that are unique within its account for as long as the gateway keeps the job. One account is one server: the connection's approved base URL without trailing slashes (for OpenAI, its fixed base), whatever connection record or credential reaches it. When the server answers a create with an id that an earlier job of that account has (a deleted one too, since its delete asks the server after the job is marked deleted), the id is not recorded, the new job ends `failed` (`upstream_id_reused`) and the create answers `502`, and every job holding that id is marked `upstream_id_reused`: the gateway never asks the server about that id again, since it may now hold another workspace's video. A marked job reads `failed` with that error (a completed one keeps the charge it settled), its content answers `video_not_ready`, its delete removes the record and the gateway's stored copy without asking the server (`upstream_deleted: false`), and one not yet settled settles with its hold kept as unknown cost, without the server being asked. A server that restarts its id counter therefore has its new jobs refused until the old ones are deleted.
> - **The output is stored, and a completed job is charged whether or not it could be.**
>   - **The charge** is made when the provider completes the video.
>   - **The download:** the poller then downloads it once into the gateway's file store: encrypted, at most 512 MiB, abandoned after 60 s without data or 25 minutes in all.
>   - **The stored copy** keeps `video/mp4`, `video/webm` or `video/quicktime` as the server labels it, and any other label as `application/octet-stream`, never sniffed. It is a `video_output` file counted toward the workspace's storage, carrying the job's key and its holder (object visibility, legal holds, subject exports). It lives `file_video_retention_days` (default 7), then `/content` answers 410.
>   - **Until then the video reads `in_progress`**, with no `completed_at`, so a client that downloads once it reads `completed` finds the output. `/content` answers the ordinary not-ready `video_not_ready` meanwhile. Logs show the real state: completed, charged, output not stored yet.
>   - **Retries:** a failed download is retried after the poll backoff until **6 hours after the video completed**. That covers an empty body, a refusal from the server (its content endpoint may lag the status), a download that keeps failing, the route disabled, the store failing, and the file store or its video outputs turned off. Past that window the last cause is recorded for good: `empty_output`, `upstream_refused`, `download_failed`, `route_unavailable`, `storage_failed` or `storage_disabled`. The window also ends a job that cannot even be refreshed.
>   - **Given up at once:** more than 512 MiB (`too_large`), or the job marked `upstream_id_reused`, before or during the download.
>   - **Who downloads:** only the poller that holds the job's claim stores the output or gives it up, both writes fenced by the claim. Its lease is renewed while it downloads, and it stops downloading once it learns its claim ended.
>   - **Poll slots:** downloads take at most half of a replica's poll slots (at least one). A job that finds none free waits for its next turn, with no attempt counted.
>   - **Outputs are stored by a poller only:** with the poller disabled on every replica, a finished video stays `in_progress`.
> - **What bounds a workspace's stored video:** a video job is created only when the file store accepts video outputs (otherwise **400 `video_storage_unavailable`**) and the workspace's stored bytes, counting 512 MiB for each of its completed videos whose output is still awaited, are under its storage quota (otherwise **400 `storage_quota_exceeded`**), both before anything is sent. The output itself is stored without a quota check, since it is already paid for, so a workspace can exceed its quota by at most 512 MiB per video it has running (its jobs at once). A stored output carries the job's key and that key's holder, so it follows the workspace's object visibility, legal holds and subject exports as the job does.
> - **Content:** `GET /v1/videos/{id}/content` serves the gateway's stored copy, never the server's copy, with:
>   - the type it was stored as, `X-Content-Type-Options: nosniff` and `Cache-Control: no-store`;
>   - always the whole body: `Range` is not supported.
>
>   What it answers otherwise:
>   - **400 `video_not_ready`** until the copy is stored;
>   - **410 `video_output_unavailable`**, with a `reason`, when it never will be. The reason is one of the causes above, `completed_before_storage` for a video completed before the gateway stored outputs, or `unknown` for a recorded cause this release does not know;
>   - **410 `video_output_expired`** after the video retention removed it;
>   - **503** with the file store unavailable, or when a stored copy's object is missing. That is data loss, logged at error, not retention.
>
>   The job itself says so too: a completed video whose output will never be available carries `output_unavailable: {reason}`.
> - **Thumbnails and spritesheets** are not stored. They come from the server while it keeps them, once the video's output is stored:
>   - at most 16 MiB, and a declared length above that is refused;
>   - ended after 30 s without data or 2 minutes in all;
>   - refused as a partial answer, a `206` or a `200` with `Content-Range`, as the video is.
> - **`expires_at`** is the server's expiry of its own copy, as it reports it; thumbnails and spritesheets are available only until then. The gateway's stored copy follows the video retention instead.
> - **Delete** is local first: it deletes the gateway's stored copy (a database failure there is an error and nothing is deleted; a file store that fails to remove the object leaves the copy unreadable at once, for the file sweeper, and the delete goes on), then marks the job deleted, which also stops any output being recorded for it and claims the deletion of one recorded since the job was read (deleted next, or by the file sweeper if the delete stops there). A deleted copy is unreadable at once; the sweeper retries an object the file store failed to remove, and also removes a stored output no job names two hours after its commit (a copy whose job was deleted, or whose claim ended, before it was recorded); a file under a legal hold keeps its object until the hold ends, as for the Files API. Then the delete asks the server to delete its own copy, best effort: the gateway's delete stands whatever the server answers. `upstream_deleted` is `true` when the server deleted it (`204`, or a `200` with a deletion object or without a JSON body; a `202`, which only queues the delete, confirms nothing) or no longer had it (`404`), or when the create was never sent; it is `false` when the server offers no delete (`405`), whose copy then stays until its own expiry, refuses or fails it, the route is disabled, or the gateway cannot ask (a create of unknown outcome, or an id marked `upstream_id_reused`); a provider copy not confirmed gone is logged, and since the job is then gone the delete cannot be retried. A deleted video is `404 not_found` from then on, to `GET` as well.

## What is supported

| Route | Behavior |
| --- | --- |
| `POST /v1/videos` | Multipart (as the official SDKs send it) or JSON. Fields: `model`, `prompt` (≤ 32 KiB), `seconds` (1 to 3600, among the lengths its routes accept; default the chosen route's first) and `size` (`WIDTHxHEIGHT`, among the sizes its routes accept). `seconds` is always sent upstream, `size` only when named. |
| `GET /v1/videos`, `GET /v1/videos/{id}` | Lists this workspace's jobs; `after`, `limit` 1–100 and `order` are supported. A single read refreshes from the provider until the job is settled. |
| `GET /v1/videos/{id}/content?variant=video\|thumbnail\|spritesheet` | Completed jobs only. `video` is the gateway's stored copy (see above for the answers before it is stored, when it never will be and after retention); `thumbnail` and `spritesheet` are streamed from the server unbuffered and bounded, as labeled among the image types, a generic binary label as `application/octet-stream`, with `nosniff`. |
| `DELETE /v1/videos/{id}` | Finished jobs only (completed or failed), because the accounting of a running job is still open. Deletes the gateway's stored copy and the record, then asks the server to delete its copy, best effort; `upstream_deleted` is `false` when the server offers no delete (`405`) or the gateway cannot ask it (see above). |
| `/v1/files` | The gateway-owned [Files API](files-api.md): `purpose=batch` uploads are stored (encrypted) in the gateway's file store; batch output and error files appear there as `batch_output`. |
| `/v1/batches` | Create, retrieve, list and cancel for `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings` and `/v1/messages` lines; see [batches](batches.md). |

**Not supported** (each returns an explicit `unsupported_capability` or `invalid_request_error`):

- Video: `input_reference` and any other option than `model`, `prompt`, `seconds` and `size` (refused by name), remix, edits, extensions and characters.
- Batch `output_expires_after`, and batch lines the endpoint itself rejects (streaming, `n > 1`, unsupported fields): see [batches](batches.md#validation-report).
- Video on OpenRouter: planned (its API has another shape and reports a cost, not seconds); OpenRouter has no Batch API.

## Identity, ownership and privacy

- Clients see gateway ids only: `video_<32 hex>`, `batch_<32 hex>` and `file-<32 hex>`. Upstream ids never leave the gateway.
- Every lookup is scoped to the calling key's workspace. Another workspace's id returns `404 not_found`, never `403`. Within a Team or Project, team and Project workspaces choose who sees files, batches and video jobs (Workspace › Settings › General, `object_visibility`). **Per user** (the default): a service-account key or the key of a workspace owner/admin sees everything; a regular member's keys see only objects created with keys issued to that member (rotation keeps access). **Workspace-wide:** every key of the workspace sees everything. In both modes only the creating user, a workspace owner/admin or a service-account key may delete or cancel (`403 permission_error`). An object the caller can't see returns the same `404 not_found` as one that doesn't exist, in lists, by id and when used as an input (`input_file_id`). Lists come from gateway rows, never the provider account, which spans workspaces.
- Reads, content and cancel need the job's connection to be enabled. Disabling a connection stops client calls and polling for its jobs.
- In Logs, a job is an ordinary request row: its request id is the execution id. The Workspace and Admin Logs filter `workload=jobs` (the **Jobs** toggle) narrows the list to video and batch jobs. Rows carry a `job` summary (`id`, `kind`, `state`, `upstream_status`), and the request page has a **Video job** or **Batch job** card. Visibility rules are those of Logs.

## State machine

Gateway states are `queued`, `in_progress`, `completed`, `failed`, `cancelled` and `expired`. A database trigger only allows forward transitions. Terminal states are final, identity columns are immutable, `settled_at` is written once and rows are never deleted.

| Provider status | Gateway state |
| --- | --- |
| Video `queued`, `in_progress`, `completed`, `failed` | Same name |
| Batch `validating` | `queued` |
| Batch `in_progress`, `finalizing`, `cancelling` | `in_progress` (the provider status is kept as `upstream_status`) |
| Batch `completed`, `failed`, `cancelled`, `expired` | Same name |

An observed regression, such as `in_progress` followed by `queued`, is ignored.

## Reservation and settlement

Both kinds use the ordinary `governance` admission and `finish`. Every budget and budget total (0015) applies unchanged, and settlement is idempotent: an identical replay is a no-op. Rate limits differ from interactive requests (see [job limits](#job-limits)).

**Video.** The workload is `videos`. Pricing v3 adds the meter `output_video_seconds_ms` (`/second`, `/minute` or `/hour`), whose `variant` is the resolution (`720x1280`, …). A line without a variant is the default.

- **Hold:** requested seconds × the highest variant rate, plus a request ceiling of 1. A meter without a price is unbounded, so admission is refused under a budget.
- **Completed:** settles from the provider's `seconds` at the reported size's line.
- **Missing or unparseable duration:** unknown; the hold is retained.
- **Longer than requested:** the evidence is kept and the hold is retained.
- **Failed:** the attempt is failed without usage. It stays unknown even when every line of the price is free or not applicable; missing token and meter observations remain NULL.

**Batch.** The workload is `batches`. Holds, batch price lists, line transfers and settlement are described in [batches](batches.md#pricing-and-budgets). A failed accepted native or legacy passthrough batch retains unknown usage/cost even on an entirely free price. A native batch settles like a job: from the provider's aggregate usage, else the sum of its decoded lines' usage. Cancelled and expired batches record usage as evidence and keep the hold. Batches created by the 0016 passthrough (`batch_mode` NULL) are still polled and settle only from the provider's aggregate usage (otherwise unknown, hold retained).

**Leases.** A job's lease runs to its poll deadline (video: 6 hours; batch: 26 hours for the default 24 h window) plus 10 minutes. A batch's reservation is admitted with that lease together with its job row, in one transaction (#640): there is no shorter admission lease, so a process that dies while creating a batch leaves either both or neither. A video's reservation is admitted with a short lease. **The job row is recorded before the provider is called** (#662): in one transaction with the extension of the lease to the poll deadline, without a provider id yet (`submit_started_at` set). If that transaction fails, nothing has been sent, so the attempt settles as a known zero and no job exists. Then the provider's create is called, with an idempotency key where the adapter supports one, and its id is filled in. The gateway therefore always has a job row, a reservation and a hold for a video the provider may have accepted; see [video without a provider id](#video-without-a-provider-id). If the id itself is lost (the id write fails, or the client disconnects while the create call runs), the provider's video cannot be polled, downloaded or deleted through the gateway, but its job ends `create_interrupted` and its hold is kept. The extension never shortens a lease. If the deadline passes without a terminal state, lease reconciliation marks the reservation unknown and keeps the hold. A later terminal observation records the job state but leaves that reservation alone.

### Video without a provider id

While its create runs, and if its outcome is never recorded, a video job has no provider id. It is never polled upstream (there is nothing to poll) and it is never shown as running work:

| Situation | Job | Reservation |
| --- | --- | --- |
| Create running, or outcome not yet recorded | `queued`, progress 0, no provider call on GET or list | pending (lease to the poll deadline) |
| The create built no request (nothing was sent) | `failed`, `create_failed` | settled at 0 (known zero) |
| The provider answered without a job (a 4xx, 429 or configuration answer) | `failed`, `create_failed` | the usual rules: unknown, hold kept, unless the free-rejection rule applies |
| The outcome is unknown (timeout, transport failure, 5xx, an unparseable answer) and no keyed resend resolved it | `failed`, `create_unconfirmed` | unknown, hold kept (the provider may have the video) |
| No outcome recorded 10 minutes after the create started (the process died, the client disconnected during the create, or the id could not be written) | `failed`, `create_interrupted`, at the first refresh after the grace (the poller or a GET) | unknown, hold kept |
| The record itself failed | no job row (a COMMIT whose acknowledgement was lost may leave one; it is ended `create_failed` at once) | settled at 0 (known zero) |

The create call is bounded at 60 s, and a record that took over 2 minutes is not followed by a create (it ends `create_failed`, known zero). A failed create records its attempt before its row ends, and the row is marked settled only if that record succeeded (otherwise a later refresh settles it, as unknown). **Resend:** at most one, in the same request, only when the adapter declares that its provider makes a create with the same key the same job (`ProviderAdapter::idempotent_video_create`), and only after an outcome-unknown failure, with the same key. The key is `omg-video-<job id>`: stable for the job, generated by the gateway, never taken from the client and not a secret. Nothing is resent from the poller or after a restart (the job table stores no prompt). The OpenAI-compatible adapter does not declare idempotent creates, so its creates are never resent. A finished video without a provider id is deleted without a provider call; its content is never available. Without a poller (`GATEWAY_JOB_POLL_INTERVAL_SECONDS=0`) and without reads, such a job stays `queued` and lease reconciliation keeps its hold as unknown at the poll deadline plus grace.

**Writers.** A video row has three kinds of writer (`store::Observer`), and every write to it checks its writer in the same statement:

- **The creating request** writes the provider id only on a `queued` row that has none, and then its first observation and settlement whatever claim exists: it holds the only copy of the create's answer. A claim cannot hold the row while a create runs inside its grace (the row is first due at the end of the grace); a claim taken after the grace that read the row without an id can then neither end nor settle it (its compare-and-set requires the id to be absent). A create that answers after a poller ended the row records nothing (the id write requires `queued`). Its own end of a row without an id (`create_failed`, `create_unconfirmed`) is the same compare-and-set.
- **The poller** writes only under its current claim: its runner, its epoch and a live lease. A claim whose lease lapsed or was released writes nothing.
- **A client read** (GET) writes only while no claim is live.

Only an observation that was recorded, with the state the row took from it, settles the job. A terminal, unsettled row whose provider now reports a different terminal state is therefore not settled from that read: it settles only from a later read of the state it has, otherwise lease reconciliation keeps its hold as unknown at the poll deadline plus grace (conflicting evidence is never charged). Ending a row without an id past its grace, and settling a terminal row whose settled mark was lost (the create's process died between the two writes), are made by the current writer only, under a `FOR KEY SHARE` hold on the job row, which no claim can take until the settlement is done. A row without an id is claimed past its poll deadline too (it is never polled upstream, only ended locally), so it never stays `queued` and unsettled for good.

Known limits:

- Jobs never fail over.
- Tiered (`min_prompt_tokens`) batch prices are applied to aggregated batch input and can over-estimate.

## Job limits

Long-running jobs do not consume interactive limits. Migration `0018_job_limits.sql` adds **Jobs at once** (`concurrent_jobs`) to every policy layer; see [governance](governance.md#jobs-at-once).

| Limit | Video and batch jobs |
| --- | --- |
| Jobs at once | Each active job holds one slot. Type defaults allow 2 per workspace. |
| Requests at once | Held from admission until the job row is recorded (for a video, right before its create call), then released: the job slot covers the create call. |
| Requests and tokens per minute | Not checked, and job reservations never count toward them. |
| Budgets | Fully applied: the conservative ceiling is reserved at admission, as before. |

A slot is released when the job reaches a terminal state (`completed`, `failed`, `cancelled`, `expired`), as soon as cancel is requested, or when the lease expires. A refused job returns `429` with `error.code` `job_limit_exceeded` (`rate_limit_error` type, retryable) and a message naming the scope kind: API key or workspace. Nothing is sent upstream.

Edit the limit with the other limits: Admin › Settings › Defaults & limits (type defaults), a workspace's platform override, Workspace › Settings › Limits (tighten only) and each key's limits. Effective access shows it per layer.

## Batch input

Batch inputs are gateway files ([Files API](files-api.md)). `POST /v1/batches` streams the file once, validates every line and copies the valid lines to the batch's private copy; see [batches](batches.md#validation-report). Limits: 50,000 lines and 4 MiB per line.

## Poller

`serve` starts a background poller on the runtime database role. It refreshes at most `GATEWAY_JOB_POLL_CONCURRENCY` jobs at once (default 4) and, every interval:

1. Claims, in one statement, as many due jobs as it has free slots that no poller holds, oldest `next_poll_at` first across two heads:
   - unsettled provider jobs still within their poll deadline: videos, native batches and 0016 batches. Gateway-run batches belong to the [batch runner](batches.md#execution-of-gateway-run-batches).
   - videos awaiting their stored output that are settled, or unsettled past their poll deadline.

   Each head reads its own partial index, never settled history.
2. Refreshes each job from its provider with a bounded deadline (31 minutes), alongside the refreshes still running from earlier intervals. A native batch is first submitted (once); when it ends, its results are collected into gateway files.
3. Applies the state machine and settles terminal jobs.

A refresh that runs for a long time (a large native collection, a slow provider) holds one slot only: the other slots keep submitting new batches, refreshing status and passing cancels on, on the same interval (#802). A freed slot is filled at once (the poller claims again as a refresh ends), and otherwise at the next interval. The claim runs beside the refreshes, so a claim that waits on the database never holds up their lease renewals, and it never takes a job this process is still refreshing, even one whose lease lapsed. A refresh that panics ends only its own work (its claim lapses after the lease). The refreshes run in the poller's own task, so a process that stops drops them and cancels their upstream requests; their claims lapse and another process takes the jobs about a minute later. A claim statement reads the head of the due-job index once per interval, including the gateway-run batches and the expired, unsettled jobs that no claim takes (lease reconciliation ends those); that read grows with the number of such jobs and is the known residual cost of the index. A claim that comes back short while jobs are due (a row another process locked or took at that moment) costs those jobs at most the time to the next refresh's end or the next interval.

**Claims (#651).** Each gateway process polls as one owner. A claim writes the owner, a 60 s lease and a bumped **claim epoch** to the job row (`async_jobs.runner_id`, `runner_lease_until`, `runner_epoch`: the columns the batch runner uses for gateway-run batches, which the poller never claims). The poller renews the lease every 15 s while it refreshes the job, beside the refresh, and gives it up when the refresh ends, so another process never polls a job one holds, and a crashed process's jobs are polled again about a minute later. A poller that stalls past its lease can lose the job to another. Until its finish of a native batch has committed, every write it then makes to the batch's row is refused, because each one checks the owner and epoch inside its own transaction while it holds the job row: it does not submit the batch, record a status, counts or the next poll, or finish the batch, and it deletes the result files it wrote. Once a renewal is refused, a result collection also stops at its next safe point (before each result line, before the lines without a result, and after its files are committed) and deletes what it wrote. After a committed finish nothing is fenced: settlement and the deletion of the provider's copies follow whatever the claim. So a poller whose lease lapses between its finish and its settlement still settles the batch (idempotently: settlement applies once whoever reaches it) and deletes the provider's copies, possibly while a second poller collects the batch again; that collection finds the batch finished, keeps the first collection's result files and deletes its own. A poller that loses its claim after a failed submission writes nothing either: the batch stays queued with its submission started, and the poller that holds it ends it `submission_interrupted` after the 40-minute grace with the hold kept as unknown cost, even when the provider's refusal would have qualified for the free release. Such a poller logs `batch poller no longer holds its batch; it stops working on it`; a failed renewal logs `async job lease renewal failed`. Once a client asked to cancel a native batch and its status reads `cancelling`, the poller keeps `cancelling` until the batch ends, whatever an earlier provider read reports. Video and 0016 batch observations are fenced too (#802): a poller's write checks its owner and epoch, so a poller whose claim ended writes nothing; a video read refreshes the job outside any claim and writes only while no poller holds the job (the poller's own observation is the newer one); progress and request counts only grow, so an older observation never lowers them; and only an observation that was recorded settles the job, with its own evidence, so a refused one never settles it with older facts. A poller that finds a started submission past its grace ends it `submission_interrupted` only if the provider's id is still absent under the finish lock. The state change stays compare-and-set along the state machine.

Failures back off per job, up to 32 × the interval. With the poller disabled, video reads still refresh and settle jobs, but native batches are not processed.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_JOB_POLL_INTERVAL_SECONDS` | `30` | Poller interval, `0`–`3600`; `0` disables the poller. |
| `GATEWAY_JOB_POLL_CONCURRENCY` | `4` | Provider jobs one process refreshes at once, `1`–`32` (validated at startup). Each refresh holds one database connection while it writes and one upstream request while it waits on the provider. |
| `GATEWAY_MAX_BODY_BYTES_VIDEOS` | 2 MiB | `POST /v1/videos` body cap (1 KiB–64 MiB). |
| `GATEWAY_BATCH_WORKERS`, `GATEWAY_BATCH_CONCURRENCY` | `4`, `2` | Gateway-run batch lines at once per process and per batch ([batches](batches.md#configuration)). Each route also has its own batch capacity and gates ([scheduling](batches.md#scheduling-on-self-hosted-models)). |
| `GATEWAY_MAX_BATCH_FILE_BYTES`, `GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES` | 200 MiB, 1 GiB | Still validated at startup but no longer used: inputs are Files API uploads and native outputs are decoded line by line. |

## Model setup and grants

- **Add model:** the **Video** and **Batch** types each have one protocol (`videos`, `batches`). A Batch model is no longer needed: any chat or embeddings model can be batched, and a Batch model runs Chat Completions natively only. Video can be chosen on `openai_compatible` connections only. A video route serves once a Platform Admin sets its video options (the clip lengths and sizes it accepts) with the route's PATCH; until then the model reads Not serving, and there is no form for the options yet. The price editor shows the video-seconds meter with resolution tiers for video models, and token meters for batch models.
- **Runtime grants** (`deploy/staging/runtime-grants.sql`): `SELECT`/`INSERT` on `async_jobs` and `async_job_files`, `UPDATE` of their state columns only (a video's `output_file_id` and `output_failure`, each written once under the poller's claim, and `output_pending`, set by the update that records the video completed and cleared with its outcome), `UPDATE(lease_expires_at)` on reservations and `EXECUTE` on `valid_upstream_job_id`. No `DELETE` or `TRUNCATE`.
- **Batch engine grants (0021):** see [batches](batches.md#storage-and-grants).
- **Probes:** `verify-privileges.sql` and the ignored `runtime_privileges` test check these grants and run a video job, native batches and a gateway-run batch as the runtime role.
