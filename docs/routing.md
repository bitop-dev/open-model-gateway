# Routing and passive health

Routing receives an already authorized, protocol/request-capability-filtered set of deployments. It does not grant model access, resolve credentials or probe providers. Every actual attempt still revalidates live workspace/key authorization and target configuration during admission.

## Defaults and configuration

A model without policy uses priority ordering, one attempt, ambiguous failover disabled and no required residency. A deployment without routing metadata uses priority 0, weight 1, unspecified residency, failure threshold 3 and cooldown 30 seconds.

The enterprise schema separates:

| Resource | Fields |
| --- | --- |
| `routing_policies` (model) | `strategy`, `max_attempts`, `allow_ambiguous_failover`, nullable `required_residency` |
| `deployment_routing` | `priority`, `weight`, nullable `residency`, **`failure_threshold`, `cooldown_seconds`** |
| `deployment_health` | `consecutive_failures`, `open_until`, `last_observed_at`, `reopen_count` (openings since the last complete success), and the probe slot: `probe_holder`, `probe_until`, `probe_epoch` |

Health is written only when it changes: a success on a route that already has no failures, no open circuit and no openings to forget (`reopen_count = 0`), observed within the last 30 seconds, writes nothing (so busy deployments are not one hot row per success); `last_observed_at` can therefore lag by up to 30 seconds. Failures and recoveries always write.

Threshold/cooldown are deployment settings, not model API fields, even though an internal routing type retains default fields. Disable a deployment through its `enabled` state, not a routing `operator_disabled` flag. Admin writes/Auditor reads use [governance API](governance-api.md).

- **Priority:** lower signed integer first; ties retain deterministic repository order (`created_at,id`). Defaults preserve first-eligible selection without retries.
- **Weighted:** strict priority tiers, weighted order without replacement within a tier. Weights 1–1000 are relative, not percentages. Workspace UUID, gateway-generated request UUID and public alias seed ordering; caller IDs are not trusted random sources.
- Open circuits and disabled model/deployment/provider rows are excluded. When cooldown is the only reason no route can serve an available model, the request fails with HTTP 503 `model_temporarily_unavailable` (OpenAI type `server_error`, Anthropic `overloaded_error`) and `Retry-After` set to the soonest reopening (for a half-open route whose probe slot another request holds, the slot's remaining time but at most 5 s), never 404 `model_not_found`. A gateway-run batch line not yet dispatched waits for the cooldown instead of failing; one already in flight when its route's circuit opens retries after its usual backoff, sends nothing while the route is still open (or its probe slot is held), and can therefore exhaust its retries and fail, as it did during an outage before (see [recovery](#recovery-one-real-request-decides)). More than 256 candidates, duplicate IDs and invalid settings fail rather than silently truncate. Repository fetches 257 to detect overflow before adapter filtering.
- A route plan is not permission to execute every candidate. Engine enforces attempt count (1–3), available IDs, one shared hard deadline and separate admission/accounting.

## Failover and residency

Retries are opt-in with `max_attempts>1`. Every fallback requires the **same explicit residency label** as the first selected target. An unspecified first target permits no fallback, even to another unspecified target. Required residency fences all primary/fallback choices, including when preferred routes enter cooldown.

Labels are 1–64 lowercase ASCII letters/digits and `._-`, starting with a letter/digit. API null means unspecified; the literal `unspecified` cannot be supplied as a required/asserted label. Labels are operator assertions, not verified provider geography or a derivation from provider names/regions.

| Failure before stream return | Eligible for another attempt? |
| --- | --- |
| Provider Busy | With attempts remaining |
| Upstream unavailable / ambiguous transport | Only with `allow_ambiguous_failover:true` and attempts remaining |
| Nothing sent: the route failed before any request to the provider was built (an unresolvable credential or endpoint, a provider SDK that found no identity), with class Busy, Overloaded, upstream unavailable or provider configuration | With attempts remaining, without `allow_ambiguous_failover`: it is not ambiguous |
| Timeout, invalid output/request, unsupported capability, storage/configuration failure | No |

"Nothing sent" is proved by the gateway, not inferred from the error: every provider request is built through one client type, which marks the attempt when it builds one (see [governance](governance.md#settlement-and-holds)). The same classes after a request was built (an upstream 401 is a configuration failure too) follow the rows above. Each attempt is admitted and reserved on its own, and the unsent one settles at zero. Ambiguous failover may duplicate upstream work or charges. It is not idempotent delivery. No fallback occurs after a provider stream is returned, even before the first event is polled. Deadlines do not restart between attempts. Local capacity/admission rejection is not a provider-health observation.

## Passive observations

While the circuit is closed, validated completion resets failures; while it is open or half-open only the probe decides (see [recovery](#recovery-one-real-request-decides)). Provider Busy, Overloaded, UpstreamUnavailable and provider-operation Timeout increment the deployment's failures; reaching its threshold while the circuit is closed or half-open opens it (see [recovery](#recovery-one-real-request-decides)). A failure while it is open changes nothing more. Invalid requests/output, unsupported options and local configuration/storage/accounting errors leave observations unchanged. Opening a stream is not success; terminal validated completion is required. Local cancellation/deadline inactivity is not a provider failure.

Failures inside a returned stream are observed too, by the class they have ([provider adapters](provider-adapters.md#failures-inside-a-returned-stream)): an error the provider reports inside the stream is classified like the same error as an HTTP response (an overload, rate limit or server error counts; a client-fault or configuration error does not), and a transport failure (a reset, a truncated stream, or corrupt Bedrock binary framing) is `upstream_unavailable` and counts. A frame that arrived whole but is malformed is `invalid_upstream_response` and does not. A client that disconnects mid-stream is a cancellation, never a provider failure. No failover follows any of these: the response was already returned.

SSE resets already counted before #622. EOF before the terminal frame now counts too: an OpenAI-compatible local server omitting `[DONE]` was already invalid, but three consecutive such streams now open the default breaker. Complete malformed SSE lines, including invalid UTF-8, do not count. A provider-reported in-stream 500 counts even when model output caused it; gateway-side network resets count too. Bedrock's active HTTP-body read bound resets on byte progress and accepts queued bytes when a reader resumes. Its stream lifetime belongs to the engine watchdog; engine deadline cancellation stays health-neutral.

PostgreSQL upsert/row locking prevents lost failure increments; counts saturate at signed-32 maximum. Concurrent completions are observed in database order, so while the circuit is closed an in-flight success can reset other recorded failures; once it opens, they change nothing. Deployment thresholds are read at recording time.

Existing plans and running attempts are snapshots, not retroactively revoked by health changes.

## Recovery: one real request decides

When a circuit's cooldown has passed, the route is **half-open**: traffic does not resume at once. The next request that can be a probe uses it alone, and its outcome decides.

- **One prober per route, across replicas.** The request claims the route's probe slot on its health row with one conditional update. The slot ends on its own after the request's timeout plus the 3 s settlement deadline (`GATEWAY_REQUEST_TIMEOUT_SECONDS` + 3), so a request that is dropped, a process that is killed, or a health write that never lands cannot keep the route closed. Every other request treats the route as cooling down: it uses another route of the model if the plan has one (this is not an attempt; nothing was sent), or gets 503 `model_temporarily_unavailable` with `Retry-After` set to the slot's remaining time but at most 5 s, since a probe usually ends in seconds. A gateway-run batch line not yet dispatched waits, as for a cooldown; a lane head that found the slot busy is planned again after 10 s (`REPLAN`).
- **The decision.** While the circuit is open or half-open, only the probe's result changes it: a result carrying the slot's latest claim, while the slot has not been freed. A probe whose slot expired with nobody claiming it again still decides; it is the only request that was let through. A complete success (a whole response, or a stream's valid terminal event) closes the circuit. A failure that counts (above) reopens it; a counted failure below the threshold (raised while the route was half-open) leaves it half-open. Either way the decision frees the slot. A result without the slot (an attempt admitted before the circuit opened) changes nothing. An outcome that is no health signal (a client error, a cancellation, a refusal at admission) frees the slot for the next request without deciding; so does a probe that ends at the request deadline, as a deadline timeout never counted before. A failure before anything was sent counts by its class, exactly as on a closed circuit: a route that cannot be reached reopens.
- **Streams.** A stream closes the circuit at its **first content**: text, a tool call, or reasoning (a reasoning delta or a Messages thinking block). A role chunk, a start event or a keep-alive does not count. A long stream therefore does not keep other requests out; if it fails later, that is an ordinary failure. A speech (audio) stream is a workload stream with no content events: it holds the slot to its end, and its end decides.
- **Backoff.** Each opening lasts the deployment's `cooldown_seconds`, doubled once per earlier opening since the route last served a complete success, capped at 300 s, or at `cooldown_seconds` itself where that is longer. With a real request as the probe, the cap is how long a recovered provider can stay excluded: for a model with one route, how long its clients keep getting 503. A complete success forgets the openings; a stream's first content closes the circuit but does not.
- **Who can be the probe.** Requests of the kinds that claim the slot: chat, Responses and Messages generation, the other workloads (embeddings, images, audio, rerank) and gateway-run batch lines. A batch retry, and a batch line dispatched while the route was half-open, is sent only as the route's probe. A line dispatched without route capacity (no route could take it) is refused at admission whatever the route's state, so it neither claims nor waits.
- **Kinds that never claim.** Token counts, native job submissions and realtime sessions reach a half-open route as they reached an expired cooldown before. Token counts and native job submissions record no health. A realtime session records its provider result: it counts as any result while the circuit is closed; on a half-open circuit it decides only while no request holds an unexpired probe slot (a success closes it, a counted failure reopens it with the backoff), so a route only realtime sessions use keeps a working breaker; it never changes an open circuit, nor a probe's decision.
- **Claimed before admission.** The slot is claimed before the request is admitted, so a request that admission then refuses (a budget, a limit, a key's restrictions) holds it for the length of its admission. A key that sends many refusable requests can therefore delay a shared route's recovery by that much each time; the slot is released at once and the next request claims it.
- **No active or synthetic probes, for any provider.** The gateway never sends a request of its own to test a route: every upstream attempt needs its own durable reservation, and there is no installation-wide scope to bill a synthetic one to. A provider's health or model-list endpoint would not prove that inference or its credentials work either. Local endpoints (vLLM, SGLang, Ollama) recover the same way as cloud ones.

**Statements on a healthy route.** A route whose circuit never opened costs what it did for an interactive or workload request's first attempt: no probe statement and no additional read. A failover attempt, a batch line's dispatch and each batch attempt's send read the route's circuit as it is then (`route_state`): through the per-replica health cache, at most one read per route per second and replica, or one `SELECT` each with `GATEWAY_CONFIG_CACHE=off`.

Management health includes failure count, nullable last observation and cooldown timestamps. No row/observation means unknown, not healthy. A closed circuit means eligible, not verified uptime; an expired one is half-open. No external probes or provider response bodies appear in these views.

## Cancellation and evidence

Deadline watchdogs drop upstream transport even with unpolled downstream streams. Dropping transport cannot guarantee upstream cancellation or zero cost; expired leases later become unknown holds rather than refunds. See [governance](governance.md).

Current implementation is `apps/gateway/src/routing.rs` plus engine/admission code. Unit and disposable-PostgreSQL tests are present; commands such as `cargo test -p open-model-gateway --lib routing::tests` do not themselves establish a fresh pass here. Historical routing milestones in [verification](verification.md) refer to earlier builds, not validation of this enterprise lineage or production load.
