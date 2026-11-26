# Guardrails design (issue #36)

**Status:** phase 1 (#840) in progress. Implemented: the policy model of §5 (migration 0092) and input enforcement: the pattern detector, the engine step for generation and embeddings, the client errors of §6, `guardrail_events` (migration 0095), the two metrics and Logs. Response text is not inspected. The dashboard pages follow. Phases 2–7 are #841–#846. **Date:** 2026-10-13; implementation notes 2026-11-25 ([§14](#14-implementation-notes-phase-1)).

*User decision* marks the user's decisions of 2026-10-13. **Recommendation** marks proposals that still need review. Today the gateway never inspects prompt or response text.

## 1. Goals and non-goals

**Goals**

- Protect the following, built one at a time and each verified before the next (*user decision*):
  - leaked secrets;
  - personal data (mask or block);
  - content moderation;
  - output DLP;
  - prompt-injection signals.
- Off by default. Opt-in per workspace, with optional platform defaults per workspace type, composed the way limits compose today ([governance](governance.md)).
- State plainly what is inspected, by which detector, and where text goes when the detector is a model.
- Actions are monitor, redact and block, configured per policy (*user decision*).
- Store no matched text. On a match, record only policy, detector, category and count (*user decision*).
- Keep every accounting invariant in AGENTS.md: one reservation per upstream attempt, unknown cost keeps its hold, no failover after a stream is returned, no implicit retry.

**Non-goals**

- Storing or replaying prompts or responses, including as "evidence" of a block.
- Inspecting images, audio bytes or files at rest. These are *uninspectable* (section 6).
- A guarantee. Pattern detectors miss things and model detectors are probabilistic. Guardrails reduce risk; they are not a compliance certificate.

## 2. Threats and use cases

| Protection | Threat | Typical action | Detectors |
| --- | --- | --- | --- |
| **Secrets** | Credentials pasted into a prompt or tool result reach a third-party provider: cloud keys, tokens, PEM keys, JWTs, connection strings, the gateway's own `omg_…` keys (`apps/gateway/src/auth.rs`). | block | pattern |
| **Personal data** | Email addresses, phones, cards (Luhn), IBANs (mod-97), national IDs or IP addresses go to a provider with an unknown data policy. | redact or block | pattern; names and addresses need System One or the judge |
| **Moderation** | Abusive, self-harm, sexual or violent content, in either direction. | monitor or block | System One, LLM judge |
| **Output DLP** | The model echoes secrets, personal data or configured confidential terms. | monitor, stop or block | pattern, System One |
| **Prompt-injection signals** | Instructions hidden in retrieved documents or tool results. | monitor, optionally block | pattern heuristics, System One, judge |
| **Custom DLP** | Code names, customer identifiers, internal hostnames. | any | pattern (admin-defined) |

Output DLP is not a separate detector. It is the output side of the secrets, personal-data and custom rules.

## 3. Where checks run

### Input

The frontends (`protocols/chat_completions.rs`, `responses.rs`, `messages.rs`) normalize requests into the canonical `ChatRequest` (`inference/types.rs`). `Engine::execute_protocol` (`apps/gateway/src/inference/mod.rs`) then: (1) acquires a capacity permit, (2) looks up `deployments`, (3) filters `candidates`, (4) builds the `route_plan`, and (5) for each attempt calls `repository.admit` (durable reservation), then the adapter, then for streams the `tracked` wrapper, which settles on `Done`.

**Recommendation:** inspect input inside the engine, after step 3 and before step 4, once per root request. Reasons:

- **One place.** Chat, Responses, Messages and gateway-run batch lines all pass through it; `jobs/runner/line.rs` calls the same `execute_protocol`.
- **Cheap failures first.** `model_not_found` and unsupported capabilities fail before any detector is paid for.
- **No reservation for a blocked input.** Inspection happens before the first `admit`, so a blocked input reserves nothing and is charged nothing upstream. Admission sees the request that is actually sent, including any redaction.
- **Same input on failover.** Attempts 2–3 reuse the inspected request, since each attempt already gets `request.clone()`. Input is not inspected again.

`Engine::execute_workload` (`inference/workload.rs`) gets the same step at the same point, after `candidates` and before `route_plan`. A new `Workload::inspection()` method defaults to "uninspectable"; phase 1 implements it for embeddings (later phases add rerank, System One and speech input).

### Detector calls: admission, billing, capacity

Model-based detectors (*user decision*) call a model the gateway serves:

- System One through `execute_workload` with a `SystemoneRequest`;
- the LLM judge through `execute_protocol` with a `ChatRequest`.

Each detector call is therefore its own root request, with its own route plan, reservation, settlement and execution row. It sends text to that model's provider.

**Recommendation: detector calls are billed to the requesting key's workspace and key lineage (the same `Principal`).**

- **Cost follows the cause.** Cost reports, cost-center snapshots and budgets stay accurate.
- **No global scope.** A platform-owned "guardrail workspace" would be one scope that every admission touches. That is the global row AGENTS.md forbids ([governance](governance.md#no-installation-wide-limits)).
- **Limits see the real work.** Budgets, tokens per minute and requests at once all include detector traffic.

**Bookkeeping.**

- Detector calls get a fresh `root_request_id`, because `inference_executions` enforces `UNIQUE(root_request_id, attempt_number)` with attempt numbers 1–3.
- A new nullable `guardrail_parent_request_id` links each detector call to the client request.
- An engine-internal flag stops detector calls from running guardrails themselves.

**Recommendation: model authorization uses a detector variant of the lookup that skips only the key's model restriction.**

Both the candidate lookup (`live_deployments`, `inference/repository.rs` ~427) and live admission (`admit_unobserved` in `governance.rs`) check `key_model_restrictions`/`key_model_selections`, so a detector call with the client's `Principal` would be refused for any restricted key. The engine (never the client) passes `purpose: guardrail`, and both queries then omit only that predicate. Key revalidation, `workspace_model_allowed` (catalog or direct assignment), the enabled checks and every limit stay. The candidate cache key (`cache.rs`, `(workspace, key, model)`) gains the purpose; `forget_key` still evicts by key.

Why not apply the restriction: it states which models the *holder* may choose, while an administrator chose the detector model. Restriction lineage is also immutable, even across rotation ([management API](management-api.md#membership-invitations-and-credentials)), so applying it would permanently break fail-closed model rules for existing restricted keys.

If the model is unavailable to the workspace, the detector fails (section 4).

**Recommendation: capacity is a parameter.** Both entry points take the shared engine semaphore with `try_acquire_owned`:
- `execute_protocol` at `mod.rs` line 124;
- `execute_workload` at `workload.rs` line 367.

The parent request already holds a permit from it, so under saturation detector calls would fail as `Busy`. Both entry points should take the semaphore as a parameter. Public calls pass the engine's semaphore; detector calls pass a separate bounded guardrail semaphore (`GATEWAY_GUARDRAIL_MAX_CONCURRENT`).

**Known cost.** A detector can be paid for and the client's own admission still be denied, for example when the budget runs out in between. Avoiding this would mean reserving first, which contradicts "a blocked input reserves nothing". Running pattern rules first, and model rules only when still needed, limits it.

### Output

Only the attempt whose output is delivered is inspected. Failover happens only on `Busy`/`UpstreamUnavailable` before anything is returned (`routing::may_failover`), so that attempt is always the last one.

The new guardrail errors are added to neither `may_failover` nor `affects_health` (`routing.rs`), so:
- an output block never fails over or retries;
- it never marks a route unhealthy.

## 4. Detector interface

One internal trait, implemented by built-in and later external detectors (*user decision*):

```rust
#[async_trait]
trait Detector: Send + Sync {
    fn kind(&self) -> DetectorKind;      // Pattern | Systemone | LlmJudge | External(name)
    fn egress(&self) -> Egress;          // Local | Model { public_model } | Service { name }
    fn categories(&self) -> &[Category]; // secret, personal_data, moderation, prompt_injection, custom
    async fn inspect(&self, ctx: &DetectContext, input: &Inspection<'_>)
        -> Result<Findings, DetectorError>;
}
```

**Phase 1** (§14): the pattern detector is the only implementation, so the trait is synchronous: `fn inspect(&self, ctx: &DetectContext, input: &Inspection<'_>) -> Result<Inspected, DetectorError>`, where `Inspected` is the distinct-value count per category plus whether the scan was `complete`, and `DetectContext` carries the request's custom-pattern budget and the built-in shapes' findings, which every rule of the request shares. The asynchronous form above arrives with the first model detector.

`Inspection` is a list of borrowed text segments, each with message index, role, part (`content`, `tool_arguments`, `tool_result`, `system`, `tool_description`) and a *new* flag (after the last assistant turn). `DetectContext` carries the principal, parent request ID, deadline and rule settings. `Findings` lists `{category, segment, span, score?}`: spans exist only in memory for redaction, and scores are compared with per-rule thresholds.

**Built-in detector kinds**

- **Pattern.** Rules plus checksums (Luhn, mod-97, key formats).
  - Runs on Rust's `regex` crate, which is linear time, so there is no catastrophic backtracking.
  - `regex` would be a new direct dependency. Today `regex-syntax` is in `Cargo.lock` only as a transitive dependency.
  - Free, and nothing leaves the gateway.
- **System One.** A `SystemoneRequest` (`inference/workload_types.rs`) with the selected text as `state` and one `noul` question per category. `valid_for` validates the answer probabilities; output is reserved to the price's `output_token_limit` (`OutputReservation::PriceCeiling`).
- **LLM judge.** A chat model with a rubric.
  - `ChatRequest` has no structured-output field ([protocol matrix](protocol-matrix.md#chat-completions)). The verdict therefore comes from one function tool, `report`, forced with `tool_choice: Function("report")`; both already exist in the canonical request.
  - The judged text is wrapped in delimiters as data. The verdict is validated strictly.
  - `max_output_tokens` is small, so the hold stays small.

**Timeouts.** `timeout_ms` per rule: default 5 s, maximum 30 s, and never beyond the request deadline (`EngineLimits::request_timeout`, 120 s by default). This applies to model and external detectors. Pattern rules take neither `timeout_ms` nor `threshold` (a write that sets either on a pattern rule is a plain 400): they do not score, and their cost is bounded by the request's custom-pattern budget below.

**Failure mode.** Each rule sets `on_error` to `fail_closed` or `fail_open`.

**Recommendation:** default to `fail_closed` for block and redact rules, and `fail_open` for monitor rules. A monitor rule that fails records `detector_failed` and the request continues.

Under `fail_closed`, the client sees the following. **Recommendation:** only transient failures are retryable. SDKs retry 5xx responses, and each retry pays for the detectors again.

| Detector failure | Client sees |
| --- | --- |
| Timeout, `Busy`, `UpstreamUnavailable`, guardrail pool full | 503 `guardrail_unavailable`, retryable |
| `RouteCoolingDown` | 503 `guardrail_unavailable` with `Retry-After` |
| Admission denied (budget, unresolved usage, token reservation) | that denial unchanged, with its existing `x-should-retry: false` |
| Model unavailable or unsupported, configuration, `price_unbounded`, invalid verdict | 503 `guardrail_detector_failed` with `x-should-retry: false` |
| Input above the detector's size cap (model detectors; phase 4) | 400 `guardrail_input_too_large` with `x-should-retry: false` |

**Unavailable versus detector failure (phase 1).** Two different conditions, never confused:

- **Unavailable** means the gateway could not finish the inspection *now*. There are four causes: no scan slot was free within the bounded wait, the workspace already has its bound of requests waiting for a slot, the custom-pattern budget ran out, or the policy could not be read (a stored rule this release cannot parse). It is always 503 `guardrail_unavailable` with `Retry-After: 1` and is retryable, whatever the rule's `on_error`. When no slot was free, nothing was scanned: no decision row is written for a rule that could run, like an admission `Busy`. A rule this release cannot run still decides under its `on_error` and is recorded. A request is never forwarded uninspected because the gateway was busy, and a transient condition never answers with the non-retryable `guardrail_detector_failed`. Matches found before a budget ran out still count: a blocking rule that found one blocks (400).
- **Detector failure** means a rule this release cannot run: a later release's detector, action, output mode or workload, or a stored pattern that no longer compiles. It follows the rule's `on_error`. `fail_closed` gives 503 `guardrail_detector_failed` with `x-should-retry: false`. `fail_open` continues and records `detector_failed`.

**Capacity (phase 1, per replica, `guardrails/limits.rs`).**

| Bound | Value |
| --- | --- |
| Scan slots | Two pools of 8: one for policies with local-layer (user-written) custom patterns, one for every other scan that leaves the async workers (an input over 64 KiB, or any custom-pattern scan). Built-in-only scans of smaller inputs take no slot. |
| Per workspace | At most 2 slots of a pool at once, and at most 4 requests in the step per pool (running or waiting). A fifth is refused at once with `guardrail_unavailable`. |
| Wait for a slot | At most 1 s, never the request deadline. Then `guardrail_unavailable`. |
| Custom-pattern budget | 250 ms per request for all custom patterns, checked before every regex call (each call looks at most 4 KiB ahead). |
| The one per-key bound | A key with 600 requests in the current minute on a replica whose decisions wrote rows and that were never admitted gets 429 `guardrail_rate_limited` before any scan, with `Retry-After` set to the end of the minute. Never admitted means refused (by guardrails, a block included, or by admission), failed before an attempt, or abandoned before one. The step runs before admission, so this bounds the rows a key can write while its requests are refused, and nothing else counts such a request. Blocking part of a key's traffic is the normal case, and a block alone never turns into a key-wide 429 below this bound. Once an attempt is admitted the request never counts, whatever happens upstream: it is bounded like any admitted request, by the rate, concurrency and budget limits that apply to it. A request none of whose routes could be tried (all cooling down) was never admitted and counts. A `/v1/messages/count_tokens` request and a native batch's create are never admitted, so they count when their decisions wrote rows. A request refused because no slot was free scanned and wrote nothing, so it does not count: a saturated replica never throttles clean traffic. Gateway-run batch lines neither count nor are refused (their batch was accepted with a bounded line count, and a clean line never fails because others were refused). The minute is a fixed wall-clock minute (counts restart at each minute boundary), so a key can reach up to twice the bound across a boundary. At most 65,536 keys are tracked per minute; a key beyond them is not throttled. Requests in flight at once all pass the check, so a key can exceed the bound by its concurrency, and a workspace with many keys multiplies it. Worst case: 600 requests × 16 rows × 1,440 minutes, about 13.8 million rows per key per day per replica (N replicas: N times that). A throttled request writes no row; `gateway_guardrail_unscanned_total{reason}` counts it (`rate_limited`), and also a request refused because no slot was free (`no_slot`). |
| Decision rows | At most 16 `guardrail_events` rows per request: the refused decision first, then the others. The number left out is recorded on the first row (`omitted`). |
| Pattern compiles | A rule's custom patterns compile once per replica and rule revision, shared by every workspace's policy (§14, "Cache"), in one slot for platform layers and two for local layers. A policy waits at most 1 s in all for slots, then gets `guardrail_unavailable`. Concurrent loads of the same rule wait for its one compile. A compile runs in a task of its own: it ends, keeps its slot until then and is kept, even when the request that started it is gone. |
| Gateway-run batch lines | A line refused because nothing could be scanned now (no scan or compile slot was free, or the policy could not be loaded) is tried again after the error's delay, doubling up to 8 s, without spending an attempt; such a refusal writes no row. After 10 such tries it spends the batch's ordinary retries (`omg_retries`, with their own backoff) like a busy scope, then fails with `guardrail_unavailable`. A line whose custom-pattern budget ran out (a wall-clock budget, which a busy replica can exhaust too) gets two more tries of its own. A try that will be tried again records nothing, so Logs and the totals show only the try that answered: the last one, or the one that ran (one attempt of a line writes at most 16 rows; a line with `omg_retries`, at most 2 more attempts each inspected again, at most 3 × 16). While it waits, a line gives its process worker back, so waiting lines never keep other batches' lines from running. It keeps its batch's permit, its route claim and its `running` state, for up to about 75 s per attempt (63 s of waits plus up to 1 s per slot wait), so it counts against the route's batch concurrency meanwhile and a batch never has more lines claimed than its concurrency. A batch stopped during the wait runs nothing more. A replica that stops during the wait leaves the line `interrupted` (recovery's rule for a running line), although nothing was sent. |
| Memory of a local policy | A local layer holds at most 50 patterns. Each compiles to at most 64 KiB, with a lazy-DFA cache of at most 128 KiB per scan using it at once (1 MiB on platform layers). A workspace runs at most 2 scans at once, so one cached local policy holds at most about 50 × (64 KiB + 2 × 128 KiB) ≈ 16 MiB of regex state. A full cache falls back to the slower engine, which the request's budget bounds. |

The pattern detector reads the whole inspected text. Its input is bounded by the inbound body caps (2 MiB for generation, `GATEWAY_MAX_BODY_BYTES_*` for other workloads), so phase 1 never answers `guardrail_input_too_large`.

Using `x-should-retry: false` on a 503 has a precedent: the existing `price_unbounded` (`protocols/mod.rs`). The comment there notes that the official OpenAI and Anthropic SDKs honor the header.

**External services later** (OpenAI moderation, Bedrock Guardrails, Presidio) implement the same trait with `Egress::Service`. They need an approved HTTPS endpoint and a credential reference under the provider-connection rules (allowlisted destinations, no redirects, proxies or implicit retries), plus a price when billed. Nothing else changes.

## 5. Policy model

### Storage and layers

Layers mirror the limit layers (*user decision*):
- a workspace-type default;
- a platform per-workspace override that replaces it;
- a workspace local layer, tighten-only, set by shared-workspace owners and admins, and by the owner of a personal workspace.

Storage is two tables: `guardrail_rule_sets` (layer, scope, revision, and `local_only` on platform layers only) and `guardrail_rules`.

*User decision:* personal-workspace owners may **tighten** their own guardrails through the local layer. Unlike limits (`personal_limits_platform_controlled`), this cannot loosen anything the platform layers set, so it needs no platform control.

*User decision:* no key layer at first. A tighten-only key layer (for example agent keys on monitor while app keys redact) can be added later if needed.

```json
{"id":"<uuid>","name":"Secrets","protection":"secrets",
 "detector":{"kind":"pattern","categories":["secret"]},
 "input":{"action":"block","redact_scope":"user_text"},
 "output":{"mode":"monitor"},
 "threshold":null,"on_error":"fail_closed","timeout_ms":null,
 "workloads":["generation","embeddings"],"uninspectable":"deny"}
```

### Composition (tighten-only)

The effective policy is the platform layer's rules plus the local layer's rules.

**Every rule runs independently**, with its own detector, threshold and failure mode.

**Per request, the strictest outcome wins**, in this order: block > redact > monitor.
- When several rules redact, their spans are combined.
- The response is handled with the strictest output mode among the rules that apply: off < monitor < stop-mid-stream < block-before-sending.

**Tightening.** A local layer may add rules and may tighten a platform rule (stronger action or output mode, `fail_closed`, lower threshold). It may never weaken or remove one.

**Egress.** `local_only` is a platform-layer field only.

When it is set, no rule whose detector sends text out (System One, judge, external) is effective. This is enforced in two places:
- **On write:** 403 `guardrail_egress_not_allowed`.
- **When the effective policy is composed:** a local rule written before the platform turned `local_only` on becomes inactive, and GET shows it with `inactive_reason:"egress_not_allowed"`. It is never silently dropped.

### Caching

Limit policies are not cached ([governance](governance.md#caches-and-change-notifications)), but guardrail rules are read on every request.

**Recommendation:** cache the effective rule set per replica under a new `guardrails` notification topic, added by a migration. It inherits the existing invalidation behavior:
- milliseconds through LISTEN, at most about 1 s through polling;
- caches are bypassed when versions can't be confirmed for 3 s.

The check takes no admission locks.

### Management API sketch

All routes use the management envelope `{error:{code:"<status>",message,reason?}}`.

| Route | Who |
| --- | --- |
| `GET/PUT /platform/workspace-types/{kind}/guardrails` | Admin writes; Auditor reads |
| `GET/PUT/DELETE /platform/workspaces/{ws}/guardrails` | Override. DELETE restores the type default. |
| `GET/PUT /workspaces/{ws}/guardrails` | PUT: shared owner/admin, and a personal workspace's owner. GET: members too (and platform staff on a Team or Project); also returns `effective`, provenance (`platform.source`, each entry's `source`) and `egress`. |
| `GET /platform/guardrails/detectors` | Built-in kinds, categories and egress |

Rejection reasons:

| HTTP | `reason` |
| --- | --- |
| 403 | `guardrail_loosen_not_allowed` (adds `rule`) |
| 403 | `guardrail_egress_not_allowed` |
| 400 | `guardrail_detector_unavailable` |
| 400 | `guardrail_pattern_unbounded` (section 7) |
| 400 | `guardrail_pattern_invalid`: a custom pattern does not compile within the limits ([§14](#14-implementation-notes-phase-1)) |

Changes are audited as `guardrail_policy.updated`, with layer, rule IDs, actions and modes. For custom patterns only their count is recorded, never their content.

### Dashboard

- **Workspace › Guardrails:** effective rules with provenance, each with one sentence on what is inspected and where it goes ("New user messages are sent to `company/jev` (OpenRouter) for moderation."), plus "Streaming responses are held until checked" when block-before-sending is on.
- **Admin › Guardrails:** type defaults and overrides.
- **Logs:** a `guardrail=monitored|redacted|blocked` badge and filter; request detail lists findings and linked detector requests with cost.
- **Usage & costs:** a `purpose: client|guardrail` breakdown.

## 6. Client-visible behavior

Errors keep the shapes in `protocols/mod.rs`:
- OpenAI: `{error:{message,type,code,param}}`;
- Anthropic: `{type:"error",error:{type,message}}`.

**Recommendation:** both shapes gain one `guardrail` object:

```json
{"error":{"message":"Blocked by a guardrail policy; error.guardrail names the rule and the category",
 "type":"invalid_request_error","code":"guardrail_input_blocked","param":null,
 "guardrail":{"policy_id":"<uuid>","rule_id":"<uuid>","stage":"input","category":"secret","detector":"pattern"}}}
```

Block errors name the rule and the category family by id and code in `error.guardrail`; the message is fixed text, because a rule's name is administrator text. They never contain matched text, offsets or scores. The new `InferenceError` variants carry only IDs and fixed enums, so the type stays `Copy`.

| Case | HTTP | OpenAI `code` / Anthropic `type` | Retry |
| --- | --- | --- | --- |
| Input blocked | 400 | `guardrail_input_blocked` / `invalid_request_error` | `x-should-retry: false` |
| Output blocked (non-stream) | 400 | `guardrail_output_blocked` / `invalid_request_error` | `x-should-retry: false` |
| Enforcing rule on an uninspectable workload or part (an image, audio or encoded document: the content could not be inspected, nothing was matched) | 400 | `guardrail_not_supported` / `invalid_request_error` | `x-should-retry: false` |
| Inspection unavailable now (section 4) | 503 | `guardrail_unavailable` / `overloaded_error` | `Retry-After: 1`, retryable |
| Detector failure under `fail_closed` (section 4) | 503 | `guardrail_detector_failed` / `api_error` | `x-should-retry: false` |
| A key's unadmitted recorded requests reached the per-replica bound (section 4) | 429 | `guardrail_rate_limited` / `rate_limit_error` | `Retry-After` |

**Allowing uninspectable content.** A rule refuses content it cannot read only while its `uninspectable` is `deny`, the default. A platform admin allows such content by setting the rule to `monitor` (it records `skipped`), or by narrowing the rule's workloads. `uninspectable: allow`, which lets a blocking rule pass content it cannot read and record `skipped`, belongs to a later phase: phase 1 refuses it on write (`guardrail_detector_unavailable`).

**Decided: output blocks are errors.** A 200 response with `finish_reason: content_filter` would be a response the provider never sent, and AGENTS.md forbids fabricated responses.

**Redaction token.** `[REDACTED:<CATEGORY>_<n>]`, for example `[REDACTED:EMAIL_1]`.
- Numbers are assigned by **first appearance in the conversation**, scanning messages in order.
- The same value always gets the same number. The value-to-number map exists only in memory for the request.
- Redaction never changes JSON structure. A match inside tool-call arguments is monitored or blocked, never redacted.

**Recommendation: redaction applies to pattern detectors only.**
- Redacting history (section 8) needs the same output on every turn.
- Model detectors are nondeterministic and, by default, judge only new content.
- So model detectors support monitor and block only.

**Per workload**

- **Chat, Responses, Messages:** all of the above. Streaming errors use each frontend's existing failure path:
  - Chat: a `data:` error body and no `[DONE]`;
  - Responses: an `error` event;
  - Messages: an `error` event.
- **Embeddings, rerank, System One (client calls), speech input:** input checks only, because their outputs are vectors, scores, answers or audio.
- **Images, transcription input, realtime:** uninspectable.
  - An enforcing rule refuses them with `guardrail_not_supported`: images and transcription before admission, realtime with a 400 before the WebSocket upgrade.
  - `uninspectable: allow` lets them through and records a `skipped` finding.
- **Batches:** gateway-run lines go through `execute_protocol`, and blocked lines get the interactive body (`batch_line_error`). Native batches run pattern rules at create time beside the existing line validation; a block rejects the batch (400 `guardrail_input_blocked`, line numbers only). Native batches needing model detectors or output blocking are refused (`guardrail_not_supported`), never silently switched to gateway-run prices. Output checks run per line at collection.

## 7. Output modes and streaming

The output mode is set per policy (*user decision*).

**Engine-side buffer cap.** Today no engine-level output bound exists:
- `Accumulator`, with its 4 MiB `OUTPUT_LIMIT`, lives in `protocols/responses.rs` and is used only by Responses and Messages;
- Chat streams without buffering;
- non-stream bodies are bounded by the adapters' 4 MiB cap.

**Recommendation:** the guardrail wrapper keeps its own text buffer with a cap, `GATEWAY_GUARDRAIL_MAX_OUTPUT_BYTES` (default 4 MiB). This buffer serves both block-before-sending and the post-`Done` model checks. When the cap is exceeded:
- under `fail_open`, checks stop and the rest is forwarded unchecked;
- under `fail_closed`, the stream is stopped as described under stop-mid-stream below, with code `guardrail_output_too_large`.

**The scanner runs inside the `tracked` loop** in `mod.rs`. If the stream were stopped by dropping `tracked`, `ExecutionGuard::drop` would record the attempt as `Cancelled` (`mod.rs` around line 464), which would be wrong.

The modes:

- **Monitor.** The stream is not changed.
  - Pattern rules scan deltas as they are forwarded. A carry-over window catches matches that are split across chunks.
  - Model rules run after `Done` on the capped buffer, in the guardrail pool. They are recorded as `skipped` when the pool is full or the client cancelled.
  - The text is dropped as soon as the verdict arrives.
- **Block-before-sending.** Events are buffered until `Done`. At `Done`, `tracked` has already settled the attempt as `succeeded`: the provider did the work. The buffer is then inspected, and either replayed or replaced with `GuardrailOutputBlocked`.
  - Frontends send only content-free opening events beforehand: Chat's role chunk, `response.created`/`response.in_progress`, or `message_start`.
  - Their existing 15 s SSE keep-alives keep idle proxies from closing the connection.
  - Clients see no tokens until the end, which is why the dashboard shows the mode.
- **Stop-mid-stream.** Text is forwarded with a holdback (default 256 bytes, configurable up to 4 KiB).
  - Every stop-mode pattern must have a maximum match length no larger than the holdback. It is computed at save time with `regex-syntax` (`Hir::properties().maximum_len()`).
  - A pattern with an unbounded or longer maximum is rejected (`guardrail_pattern_unbounded`), or rewritten to trigger on a short prefix. For example, the built-in PEM rule triggers on its `-----BEGIN … PRIVATE KEY-----` line, because the full key is 1.7 KB or more.
  - On a match the loop calls `guard.finish(Failed, Some(GuardrailOutputBlocked))`, yields the error, then drops upstream, which cancels it. The finished guard's `drop` is a no-op. Usage is usually unreported, so the hold stays unknown. Sent tokens can't be taken back.
  - Responses and Messages already emit tool calls whole ([protocol matrix](protocol-matrix.md#streaming-and-evidence-boundaries)); Chat streams tool-call deltas, so the holdback covers them.
  - **Recommendation:** pattern rules only in this mode.

## 8. Coding agents

Agents and harnesses such as Claude Code resend the whole conversation on every turn. They rely on the history staying byte-identical.

**Today**, the shared `ChatRequest` carries no cache markers, thinking blocks or signatures, but native Anthropic Messages preserves `cache_control` hints and thinking blocks with their signatures, and forwards `anthropic-beta` as an open list that refuses only a denylist ([protocol matrix](protocol-matrix.md#messages)). The design has to hold for those histories.

Ways redaction can break agents:
- **Prompt caching.** Provider caches match exact prefixes. If an earlier turn changes, cache reads become cache writes, and the prefix hash that the proposed cache-aware routing would key on changes too.
- **Thinking signatures.** Changing a signed thinking block makes the provider reject the request.
- **Tool history.** If the model sees `[REDACTED:…]` in a file it read, it can write the placeholder back through its edit tool and corrupt the user's file.

**The user's decision** recommends monitor or block for agent traffic. It also offers a redact mode that touches only new user input. Applied on its own, that mode leaks: on the next turn the agent resends the original text as history, unredacted. First-send-only redaction protects only the first send.

*User decision:* the default `redact_scope` is `user_text`.
- Every turn, the same deterministic pattern redaction runs over every user-authored text segment, earlier turns included, numbered by first appearance as in section 6.
- Earlier turns therefore produce byte-identical redacted text, and the upstream prefix and its cache stay stable. A policy change shifts the prefix once.
- Assistant turns, tool calls, tool results and thinking blocks are never touched, and any cache markers are preserved.

Even so, a value the agent reads into a tool result is still sent, so **for secrets, block is the recommended action**, not redact. `redact_scope: all`, which includes tool results, is an explicit opt-in with a dashboard warning.

- *User decision:* model detectors judge only *new* segments by default, to keep cost down. Re-judging the whole history every turn would make cost grow quadratically.
- Pattern rules, which are free, scan everything.
- Monitor mode is the "inspect without modifying" mode: nothing sent upstream changes.

## 9. Privacy and recorded metadata

- **Never stored:** matched text, spans, offsets, context, redaction maps, detector inputs, judge rationales or raw scores. A detector request is an ordinary metadata-only execution row.
- **`guardrail_events`:** a new table, partitioned by month like the rest of history. One row per decision:
  - `root_request_id`, workspace, key, stage;
  - policy, rule and revision;
  - detector kind and category;
  - `count`;
  - the action: `monitored|redacted|blocked|stopped|detector_failed|skipped`.

  A blocked input has no execution row, so this table is how Logs shows blocked requests.
- **Visibility** is the same as Logs: members see their own requests, shared admins the whole workspace, personal owners only their own, platform staff Team/Project detail only. *User decision:* platform staff also see guardrail counts for personal workspaces as totals (per action and category, no request detail), the same way they see personal usage totals.
- **Audit:** `audit_events`, which is append-only, records policy changes only, not individual decisions.
- **Metrics:** `gateway_guardrail_decisions_total{stage,detector,family,action}`, `gateway_guardrail_detector_seconds{detector}` and `gateway_guardrail_unscanned_total{reason}` (requests refused without a scan: `rate_limited`, `no_slot`).
  - Label values come from fixed sets, with custom rules counted as `custom`.
  - No workspace, key or request labels, per the rule already stated in `metrics.rs`.
- **Logs:** request ID and decision only.

## 10. Cost, latency and capacity

- **Pattern rules** are linear in the bytes scanned. Scanned input is bounded by:
  - the inbound body caps: 2 MiB for generation, `GATEWAY_MAX_BODY_BYTES_*` for other workloads;
  - for output, the guardrail buffer cap (section 7) and the scanner's carry/holdback window.

  An optional per-rule `max_scan_bytes` could lower this further, with an excess following `on_error`. It is not implemented in phase 1 and is not part of the rule shape; the pattern detector's cost is bounded by the request's custom-pattern budget and the slots (section 4). Custom patterns are limited to 50 per layer and 512 bytes each.
- **Model rules** send at most `max_detector_input_bytes` per call (**Recommendation:** 64 KiB), one call per rule and stage.
  - They add a model round trip before dispatch, so they run in parallel after the pattern rules.
- **Accounting.** Each detector call is a normal attempt (reservation, settlement, held unknown cost, cost-center snapshot) and counts against budgets, tokens per minute, requests at once and, *user decision*, requests per minute (documented: a request with detector calls can count up to three times).

## 11. Testing

All model tests use mocks: no paid calls.

- **Corpora.**
  - Positives: synthetic secrets assembled at test time, or vendor-documented example keys, so push protection never fires; Luhn test card numbers.
  - Negatives: source code, UUIDs, hashes and base64 data, to measure false positives.
- **Streaming:** every positive split at every byte boundary; no byte of a stop-mode match forwarded; unbounded patterns rejected for stop mode; monitor leaves streams byte-identical.
- **Redaction across turns:** a multi-turn transcript gives a byte-identical upstream prefix every turn; assistant and tool segments never change.
- **Model detectors** (mock System One and chat servers): verdicts, invalid verdicts, timeouts, budget denials, the retry classes of section 4, restricted keys.
- **Accounting** (real PostgreSQL): no reservation for a blocked input; detector attempts have their own linked reservations; a stop is `failed`, not `cancelled`, and keeps its hold; blocks never fail over or touch route health.
- **Privacy.** A canary value goes through every action, then every table, log and metric is scanned for it.
- **Contracts and UI.** Error bodies for the three protocols and for batch lines; browser tests for the dashboard.

## 12. Build plan

Each phase is one issue with tests, review and a PR.

**1. Pattern secrets (input).** Rule storage, layers and composition, management API, cache topic, `guardrail_events`, metrics, Logs, dashboard pages, the `Detector` trait, and secrets with monitor and block for generation and embeddings.
- *Accept when:* documented bodies on all three protocols; nothing reserved; canary test passes; local layers cannot loosen; egress is enforced when composing.

**2. Pattern personal data.** Checksummed categories, redaction (`user_text`, `latest_user_turn`, `all`) and uninspectable handling.
- *Accept when:* the upstream prefix is byte-stable across turns; assistant and tool segments are untouched; the false-positive rate is reported.

**3. Output monitor.** Scanner inside `tracked`, the engine buffer cap, post-`Done` checks.
- *Accept when:* streams are byte-identical with monitor on; findings are recorded.

**4. System One detector.** The guardrail-purpose lookup and admission variant, the semaphore parameter, failure modes and the egress display.
- *Accept when:* linkage and cost are visible; restricted keys work; the retry classes match section 4.

**5. LLM-judge detector.** The `report` verdict and strict validation.
- *Accept when:* invalid verdicts and timeouts follow `on_error`.

**6. Output blocking modes.**
- *Accept when:*
  - for stop-mode rules, whose maximum match length is at most the holdback, no byte of a match is forwarded;
  - for prefix-triggered rules, nothing at or after the trigger is forwarded;
  - outcomes match section 7;
  - batch lines carry the interactive error bodies.

**7. External-detector interface.** Approved endpoints, credential references and prices.
- *Accept when:* an external mock works through the unchanged policy model.

## 13. Decisions on the open questions (2026-10-13)

1. Detector attempts **count toward requests-per-minute** limits; this is documented.
2. **No per-key layer** at first; add a tighten-only one later if needed.
3. **Personal-workspace owners may tighten** their own guardrails (local layer, tighten-only).
4. Redaction covers **all user-authored text on every turn** (`user_text`), never assistant, tool or thinking blocks.
5. Platform staff see **guardrail counts for personal workspaces as totals**, never request detail.
6. Model detectors judge **only new content** by default, to keep cost down.

No open questions remain; implementation starts with phase 1 (section 12) after phase 2 of the program.

## 14. Implementation notes (phase 1)

Where the implementation had to choose, and what it chose (#840):

- **Tightening a platform rule.** A local layer holds its own rules and *tightening entries*: `{"tightens":"<platform rule id>"}` with only `input.action`, `output.mode`, `on_error` or `threshold`. Each field may only be at least as strict as the platform rule's (`guardrail_loosen_not_allowed`, naming the platform rule, otherwise; also for a local rule that reuses a platform rule's id). Composition takes the stricter value of each field, so an entry written against an older platform rule never weakens the current one. An entry whose platform rule was removed stays, inactive, with `inactive_reason:"tightened_rule_missing"`. The local layer's own rules are freely edited and removed by its writers: tighten-only constrains the platform's rules, not the local layer's.
- **Refusal order on write.** A shape error is a plain 400. Then `guardrail_egress_not_allowed` (403) when the rule's detector sends text out under `local_only` (set on the layer itself, or on the platform layer above a local one), before `guardrail_detector_unavailable` (400). So the stronger reason wins for a rule that could never be allowed. Phase 1 refuses every value it cannot enforce with `guardrail_detector_unavailable`: other detectors, categories other than `secret` and `custom`, `redact`, any output mode but `off`, workloads other than generation and embeddings, and `uninspectable: allow`.
- **Storage keeps the design's whole rule shape** (every detector kind, action, output mode and workload). Later phases widen what the gateway accepts, not the table. A stored rule this release cannot run (written directly, or by a later release during a rolling upgrade) is composed and shown, but never as `active` and never skipped silently (input enforcement holds this). It follows its own `on_error` as a detector failure (a block rule fails closed with `guardrail_detector_failed`; a `fail_open` rule passes and records the failure), and GET names the reason it cannot run instead of showing it `active`. In part 1 nothing is enforced and every response says `enforced: false`, so such a rule is composed and shown as stored.
- **Custom patterns.** 1 to 50 per layer, at most 512 bytes each, compiled at save with `regex` under fixed limits (compiled program 1 MiB for platform layers and 64 KiB for local layers, lazy DFA 1 MiB, nesting 64: any user can write a personal workspace's local layer, and a small program bounds the work per scanned byte when a pattern defeats the lazy DFA; Unicode classes such as `\w` or `\d` are large, so local patterns use ASCII classes like `[0-9]` or `(?-u:\w)`). Compiles run on the blocking pool in per-replica slots, one for the platform layers and two for the local layers, so local writers never queue ahead of Platform Admins; a slot is held until its compile ends, even when the client disconnects. The route policy layer refuses a caller who may not write the layer before the body is read, so a refused caller never gets a pattern compiled; one that does not compile is `guardrail_pattern_invalid`. Pattern text is configuration and is stored. It is shown to the layer's writers and, for platform layers and a Team or Project's local layer, to platform Admins and Auditors; everyone else sees `pattern_count`. Audit records only the count.
- **A PUT replaces the whole layer**, so a platform layer's PUT states `local_only` every time: an omission is a plain 400, never a stored `false` that would lift a type default's restriction for one workspace by accident. A local PUT that names it is a 400 too.
- **The database backs the writers.** A rule set records the transaction that last inserted it or moved its revision, and its rules are inserted or removed only in that transaction, so each change of a set's rules is made by the set's guarded writer and moves its revision. An override is removed only in a transaction that first moved its revision (by a Platform Admin). A set holds at most 50 entries and 50 custom patterns; a full rule carries patterns exactly when it has the `custom` category; arrays are one-dimensional. Patterns are compiled before the management transaction starts, never while it holds the installation row.
- **Personal workspaces.** The owner's local layer is an owner-made choice: platform staff see a personal workspace's platform layers (`GET /platform/workspaces/{ws}/guardrails` answers `local: null, local_visible: false, effective: null`), never its local layer.
- **Responses** carry `enforced: true` once the engine step enforces input rules (part 2), with `inspected_workloads`; output is not inspected.

Input enforcement:

- **What is inspected.** Everything a generation request sends upstream as text: messages (content, tool-call ids, names and arguments, also decoded), tool definitions (every string of their schemas), the forced tool's name, and the native Messages envelopes and options (system, stop sequences, output schema, context management, earlier thinking); every string and object key is read, signatures included (a client may send them); only encoded or remote sources are skipped. A message's text blocks and consecutive system messages are also read joined, and a text seen twice (its canonical and native forms) is scanned once. An embeddings request's inputs. A value found in several places counts once (`count` is distinct matched values; at most 10,000 matches are kept per scan). Inputs over 64 KiB are scanned on the blocking pool in the scan slots of §4 ("Capacity"). A built-in shape counts unless it is glued to a run of eight or more base64url characters (letters, digits, `_`, `-`): inside an encoded blob it is data, while punctuation, escapes (`\n`, `%20`) or a single glued character do not hide it. A run of at most 32 characters that is a *label* does not glue: snake or kebab case words, each letters of one case with digits anywhere, PascalCase, digits alone or empty, with a separator against the token (`deploy_token-…`, `…_prod`, `PROD_DEPLOY_TOKEN_…`, `K8S_DEPLOY_…`, `DeployKey_…`, `api_key_1_…`, `__secret_…`). camelCase (`…_forUnitTests`, usual in code), random mixes (`aB3dE5…`) and longer runs stay data. So a camelCase or mixed-acronym label (`myApiKey-…`, `AWSKey_…`, `OAuth2Token_…`) still hides a token (a known limit: accepting them would turn identifiers in code into findings). A labelled AWS secret key's match includes the identifier its keyword ends (`productionAwsSecretAccessKey = …`). Custom patterns are scanned on the blocking pool, each regex call looking at most 4 KiB ahead with the text before it as context (a match that ends within 1 KiB of that horizon is searched again from where the window began, up to 8 KiB past the match's start, so a longer alternative, or one starting earlier, that runs past the cut is the one counted; matches found before the budget runs out are kept), within a budget of 250 ms per request for all of them, checked before every call (tests give the budget as a number of calls). A rule whose custom scan runs out of budget is *unavailable* (§4): the request is refused with the retryable 503 `guardrail_unavailable` whatever the rule's `on_error`, and `detector_failed` is recorded. Its built-in secret shapes and the matches found before the budget ran out still decide, so a blocking rule that found one blocks. An input large enough to outrun the budget is therefore refused, never forwarded.
- **Known limits of custom-pattern scanning (phase 1).** A value longer than 1 KiB that its pattern matches only whole (a `BEGIN…END` block, "A within 2,000 characters of B") can be missed when it starts more than 1 KiB before a 4 KiB horizon, an offset the sender can choose; patterns for short values (keys, tokens, identifiers) are not affected. A single match longer than 8 KiB is counted in pieces, so its distinct count can be more than one. A built-in shape whose prefix can end an ordinary word (`sk_live_`, as in `task_live_streamingHandler`) can match inside an identifier. Some secrets have no built-in shape: a secret key labelled in prose (`Secret access key: …`) or as `AWS_SECRET_KEY`, a value longer than 40 characters after a `secret_access_key` label (a 64-character hex key: the rest is glue), a connection-string password with a raw `/`, and credentials in an `https://user:pass@host` URL or a JDBC `password=` parameter; a custom pattern covers them. Scan slots are bounded per workspace (2 of 8 per pool, 4 requests in the step) and user-written local patterns have their own pool, so one workspace cannot take a replica's inspection capacity. Four workspaces scanning at once can still fill a pool, and a fifth workspace's scan then waits up to 1 s and is refused with the retryable `guardrail_unavailable`. Built-in-only inspection of inputs up to 64 KiB takes no slot and is never delayed this way. A single regex call can overshoot the budget, by at most one call. A pattern that can match the empty string, or exceeds 512 bytes, is refused at save (`guardrail_pattern_invalid`) before it is parsed further. Images, audio and documents that are not plain text cannot be read: a request carrying one is refused by a blocking rule with `uninspectable: deny` (`guardrail_not_supported`, recorded `skipped`) and recorded `skipped` by a monitor rule. Characters are not normalized (zero-width characters or homoglyphs can defeat a pattern).
- **Paths.** Chat Completions, Responses, Messages, `/v1/messages/count_tokens` (which sends text to the provider; its decisions are recorded with operation `count_tokens`, and a refused count is listed in Logs) and gateway-run batch lines run the step for generation; embeddings and embeddings batch lines for embeddings. A native batch's lines go to the provider's own batch API and are not read in phase 1: an enforcing rule refuses the batch at create time with `guardrail_not_supported` (gateway-run mode, `metadata.omg_mode=gateway`, inspects each line), a monitor rule records `skipped`. A native batch is checked once, at create time: a rule added later does not stop its submission. Image parts of Chat Completions and Responses messages (image input, #24) cannot be inspected: a request that carries one is uninspectable, so a blocking rule with `uninspectable: deny` refuses it (`guardrail_not_supported`) before anything is admitted or sent, and a rule that does not block records `skipped`; the text parts beside an image are still read (also joined). Gateway-run batch lines refuse images when they are parsed, so no batch line carries one. Phase 1 rules can name only generation and embeddings, so rerank, System One, speech, images, transcription, realtime and video are not checked; the phases that add them add their step. A decision that cannot be recorded (`guardrail_events` unavailable) fails the request with `accounting_unavailable` (503), like admission.
- **Shared compiled detectors.** The map holds at most 4,096 rules. Each rule holds at most 50 patterns (a set's cap), and each pattern compiles to at most 1 MiB on a platform layer or 64 KiB on a local one. So the map's worst case at its cap is 4,096 × 50 × 64 KiB ≈ 12.5 GiB of programs for local rules (platform rules are written by Platform Admins only), plus lazy-DFA caches of 128 KiB (local) or 1 MiB (platform) per pattern for each scan using it at once. A typical pattern compiles to a few KiB. At the cap an arbitrary eighth of the map is evicted. Policies keep the detectors they hold (the policy cache, at most 100,000 workspaces per replica, bounds those), and an evicted rule recompiles the next time a policy that needs it is loaded.
- **Built-in secret shapes** (category `secret`): AWS access key ids and secret access keys (labelled by an identifier ending in `secret_access_key` in any case, with `_`, `-` or nothing between the words, such as `aws_secret_access_key`, `aws-secret-access-key` or AWS's own JSON `"SecretAccessKey"`, followed by `=`, `:` or `=>`), GitHub, GitLab, Slack (`xox…` tokens, and app-level tokens by their structure `xapp-<n>-<id>-<n>-<secret>`, so `xapp-container` identifiers are not findings), Stripe, OpenAI, Anthropic (`api`, `admin` and `oat` keys), OpenRouter (`sk-or-v1-`), Google, Hugging Face and npm tokens, Slack webhooks, private-key `BEGIN` lines (PGP `PRIVATE KEY BLOCK` included; the header's leading and trailing `-----` are its own delimiters, so neither a label glued before it nor a body glued after it, as in a `.env` value with its newlines stripped, hides it), JWTs, connection strings with a password (the scheme in any case, also with a driver suffix such as `postgresql+psycopg2://`, or with no user, such as `redis://:password@host`), Azure storage account keys, and the gateway's own inference keys and admin tokens. Each is anchored on a prefix or a fixed structure.
- **Error bodies** name the rule by id, not by name: the message is fixed text, and `error.guardrail` carries `policy_id` (the rule set), `rule_id`, `stage`, `category` and `detector`. A rule name is administrator text and the error type carries only ids and fixed codes (§6's example quotes a name).
- **A rule this release cannot run** (a later release's detector, action, output mode or workload) is never shown active (`inactive_reason: "not_runnable"`) and never skipped: under `fail_closed` the request is refused with `guardrail_detector_failed` (503, `x-should-retry: false`), under `fail_open` it continues and `detector_failed` is recorded.
- **`guardrail_events`** (§9) also records the requested `model`, the client `operation`, whether a stream was asked for and whether the decision is the one the request was `refused` with, so Logs can show a refused request, which has no execution row. It is archived as its own group (`guardrails`), because it has no partitions for the months before migration 0095.
- **Logs.** A refused request is listed with status `blocked` from its refusing decision (`blocked` means refused by the guardrail step: a block, a part that cannot be read, a scan that could not finish, or a detector failure; the detail's `guardrail_decisions` gives the refusing decision's action), under the same visibility as every request; every request row carries a `guardrail` summary, the detail lists the decisions, and the list, detail and metrics take a `guardrail` filter. Metrics count requests that made an attempt. Platform staff see a workspace's decisions this month as totals (per action and category) in the platform guardrail view, also for a personal workspace (§13, decision 5).
- **Cache.** A workspace's compiled policy is cached per replica under the `guardrails` and `access` topics (a workspace's kind selects its type default). A hit costs no statement. A stale or unconfirmed entry costs one read of the workspace's rule-set heads (id, layer, revision, `local_only`), and the policy is recompiled only when they changed, so a change is never served from a stale entry and an unconfirmed replica does not recompile per request. Concurrent confirmed misses of a workspace on one replica wait for one load and are then served by it; while the versions are unconfirmed every lookup reads the heads (in parallel) and only compiles wait for each other, each reading the heads again once it holds the workspace's compile turn, so the policy stored last is the latest. A rule's custom patterns are compiled once per replica and rule revision and shared by every workspace's policy (`guardrails/shared.rs`: kept under rule set, revision and rule; a newer revision of a set evicts the older ones; at most 4,096 rules, a policy keeping what it holds). So a platform edit compiles its patterns once, not once per cached workspace, and concurrent loads of the same rule wait for that one compile, in the compile slots (one for platform layers, two for local ones), off the async workers and without a database connection. The rest of a policy compiles no regex. Heads and rules are read in one `REPEATABLE READ, READ ONLY` transaction, and the policy is cached under the heads of that snapshot, so a set changed or removed between two reads can never come back as a set without its rules (an override removed mid-read would otherwise shadow the type default with an empty policy). The repository methods the engine reads it through have no default, so no repository runs requests without guardrails.
- **What enforcement covers.** Every guardrail response says `inspected_workloads: ["generation","embeddings"]`: the workloads whose input this release inspects. A blocking rule does not cover any other workload (rerank, System One, speech, images, transcription, realtime, video), even where its `workloads` could name one in a later phase.
- **Local tightenings in platform totals.** A decision of a platform rule that a local tightening entry made stricter is recorded `tightened`, because its outcome may be an owner-made choice. A personal workspace's platform totals leave those out, like decisions of its local layer; a shared workspace's totals count them.
- **`guardrail_events` references its parents by trigger.** Migration 0095 checks at insert time that the key belongs to the event's workspace, raising SQLSTATE 23503 like a foreign key (the pattern of migration 0034). It has no foreign keys to `workspaces` or `api_keys`, so concurrent decisions never take `FOR KEY SHARE` on those rows and never create MultiXacts there.
