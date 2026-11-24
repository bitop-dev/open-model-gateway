/*
 * Resolving one unknown realtime response (#804): a Platform Admin settles a response whose usage the session never
 * recorded, from authoritative evidence, valued by the server with the session's pinned price. The session settles once
 * its last unknown response is resolved. Counters are exact decimal strings; recorded counts may only be raised.
 */
import { api, wsPath, type Session } from "./api";
import type { Action } from "../components/ui";
import type { Field, Values } from "./forms";
import type { RealtimeResponse, RequestAttempt } from "./requests";

const MAX = 9223372036854775807n;
export const realtimeCounters = [
  ["input_text_tokens", "Text input tokens"],
  ["cached_text_tokens", "Cached text input tokens"],
  ["input_audio_tokens", "Audio input tokens"],
  ["cached_audio_tokens", "Cached audio input tokens"],
  ["output_text_tokens", "Text output tokens"],
  ["output_audio_tokens", "Audio output tokens"],
] as const satisfies readonly (readonly [keyof RealtimeResponse, string])[];

/** Who may resolve which response: a Platform Admin, an unknown response of a session whose cost is unknown. */
export const canResolveRealtime = (session: Session, attempt: Pick<RequestAttempt, "accounting_state">, response: Pick<RealtimeResponse, "state">) =>
  session.capabilities.platform_write && attempt.accounting_state === "unknown" && response.state === "unknown";

export function realtimeResolutionFields(r: RealtimeResponse): Field[] {
  const counter = ([name, label]: (typeof realtimeCounters)[number]): Field => {
    const recorded = r[name] as string | null;
    return {
      name, label, required: true, value: recorded ?? "",
      help: recorded != null ? `Recorded ${recorded} · may only be raised.` : "Required: the exact count from the evidence.",
      validate: v => !/^\d{1,19}$/.test(v) || BigInt(v) > MAX ? "Enter an exact non-negative integer without exponent notation." : recorded != null && BigInt(v) < BigInt(recorded) ? `At least the recorded ${recorded}.` : undefined,
    };
  };
  return [...realtimeCounters.map(counter), { name: "evidence", label: "Authoritative evidence reference", required: true, maxLength: 200, help: "No prompt, response or secret. Repeating the identical evidence and counters is idempotent; other evidence for a resolved response is refused." }];
}
export const realtimeResolutionBody = (values: Values) => ({
  ...Object.fromEntries(realtimeCounters.map(([name]) => [name, values[name]])),
  evidence: values.evidence,
});
/** The dialog and write path (`POST …/costs/{execution}/realtime-responses/{sequence}/reconcile`). */
export const realtimeResolveAction = (workspace: string, execution: string, r: RealtimeResponse): Action => ({
  title: `Resolve response ${r.sequence}`,
  description: "Platform Admin action. Enter this response's usage from the provider's authoritative record; the session's pinned price values it, and unknown rates are not free. The session's cost becomes final once every response is resolved.",
  fields: realtimeResolutionFields(r), submitLabel: "Resolve response", successNotice: "Response resolved.",
  run: (v, signal) => api(`${wsPath(workspace)}/costs/${encodeURIComponent(execution)}/realtime-responses/${r.sequence}/reconcile`, { method: "POST", body: realtimeResolutionBody(v), signal }),
});
