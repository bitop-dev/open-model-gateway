// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { PlatformRequestDetailPage } from "../pages/request-detail";
import type { DashboardSearch } from "../lib/permissions";
import { abortRequests } from "../lib/api";
import { canResolveRealtime, realtimeResolutionBody, realtimeResolutionFields } from "../lib/realtime-resolution";
import type { RealtimeResponse, RequestAttempt, RequestDetail, RequestRow } from "../lib/requests";
import { admin, auditor, testClient } from "../lib/test-fixtures";

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const ROOT = "1a2b3c4d-0000-0000-0000-000000000001", EXEC = "e1e1e1e1-0000-0000-0000-000000000000";
const response = (patch: Partial<RealtimeResponse> = {}): RealtimeResponse => ({ sequence: 2, state: "unknown", status: "completed", input_text_tokens: null, cached_text_tokens: null, input_audio_tokens: null, cached_audio_tokens: null, output_text_tokens: null, output_audio_tokens: null, cost_microusd: null, held_microusd: "44800", unbounded_cost: false, started_at: "2026-10-08T10:00:00Z", completed_at: "2026-10-08T10:00:01Z", duration_ms: 1000, ...patch });
const settled = response({ sequence: 1, state: "settled", input_text_tokens: "10", cached_text_tokens: "4", input_audio_tokens: "20", cached_audio_tokens: "5", output_text_tokens: "10", output_audio_tokens: "40", cost_microusd: "3228", held_microusd: "0" });
const row: RequestRow = { root_request_id: ROOT, workspace: { id: "team", name: "Product", kind: "team" }, started_at: "2026-10-08T10:00:00Z", completed_at: "2026-10-08T10:00:02Z", model: "company/voice", upstream_model: "gpt-realtime", key: { id: "key1", name: "Voice app" }, status: "succeeded", finish_reason: null, attempts: 1, input_tokens: null, output_tokens: null, cached_input_tokens: null, reasoning_tokens: null, cost_microusd: null, held_microusd: "48028", latency_ms: 2100, time_to_first_token_ms: null, generation_ms: null, tokens_per_second: null, cost_center: null, workload_kind: "realtime", streamed: true, session_id: null, app: null };
const attempt = (patch: Partial<RequestAttempt> & { realtime_responses?: RealtimeResponse[] } = {}) => ({ attempt_number: 1, execution_id: EXEC, state: "succeeded", error_code: null, started_at: row.started_at, completed_at: row.completed_at, latency_ms: 2100, deployment: { id: "d", upstream_model: "gpt-realtime" }, connection: { id: "c", name: "OpenAI", provider: "openai" }, input_tokens: null, output_tokens: null, billing_usage: null, meter_usage: null, cost_microusd: null, held_microusd: "48028", accounting_state: "unknown", unresolved_reason: "incomplete_or_unpriced_meter_usage", price_id: "p", pricing_version: 3, failover_reason: null, workload_kind: "realtime", realtime_responses: [settled, response()], ...patch });
const detail = (a = attempt()): RequestDetail => ({ ...row, workspace_id: "team", attempt_count: 1, attempts: [a as RequestAttempt], prev_id: null, next_id: null });

function serve(body: RequestDetail) {
  const posts: { url: string; body: unknown }[] = [];
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (init?.method === "POST") { posts.push({ url: String(path), body: JSON.parse(String(init.body)) }); return Promise.resolve(Response.json({ ok: true })); }
    if (String(path).startsWith(`/api/v1/platform/logs/requests/${ROOT}`)) return Promise.resolve(Response.json(body));
    return Promise.resolve(Response.json({ data: [], has_more: false }));
  });
  vi.stubGlobal("fetch", fetch);
  return posts;
}
const page = (who = admin): ReactNode => <QueryClientProvider client={testClient()}><ActionProvider><DashboardNavigationProvider search={{ page: "platform-log-detail", record: ROOT } as DashboardSearch} navigate={() => {}}><PlatformRequestDetailPage session={who} id={ROOT} /></DashboardNavigationProvider></ActionProvider></QueryClientProvider>;

describe("Realtime response resolution", () => {
  it("prefills recorded counters, refuses lowering them, and posts exact decimal strings", () => {
    const fields = realtimeResolutionFields(response({ input_text_tokens: "10" }));
    expect(fields.map(f => f.name)).toEqual(["input_text_tokens", "cached_text_tokens", "input_audio_tokens", "cached_audio_tokens", "output_text_tokens", "output_audio_tokens", "evidence"]);
    const text = fields[0]!;
    expect(text.value).toBe("10");
    expect(text.validate?.("9", {})).toMatch(/at least the recorded 10/i);
    expect(text.validate?.("10", {})).toBeUndefined();
    expect(text.validate?.("1e3", {})).toBeTruthy();
    expect(fields[1]!.validate?.("9223372036854775808", {})).toBeTruthy();
    expect(fields[1]!.validate?.("9223372036854775807", {})).toBeUndefined();
    expect(fields.every(f => f.required)).toBe(true);
    expect(realtimeResolutionBody({ input_text_tokens: "10", cached_text_tokens: "4", input_audio_tokens: "20", cached_audio_tokens: "5", output_text_tokens: "10", output_audio_tokens: "9007199254740993", evidence: "invoice:7" }))
      .toEqual({ input_text_tokens: "10", cached_text_tokens: "4", input_audio_tokens: "20", cached_audio_tokens: "5", output_text_tokens: "10", output_audio_tokens: "9007199254740993", evidence: "invoice:7" });
  });

  it("is offered to a Platform Admin only, for an unknown response of a session whose cost is unknown", () => {
    const unknown = { accounting_state: "unknown" };
    expect(canResolveRealtime(admin, unknown, response())).toBe(true);
    expect(canResolveRealtime(auditor, unknown, response())).toBe(false);
    expect(canResolveRealtime(admin, unknown, settled)).toBe(false);
    expect(canResolveRealtime(admin, unknown, response({ state: "pending" }))).toBe(false);
    expect(canResolveRealtime(admin, { accounting_state: "pending" }, response())).toBe(false);
    expect(canResolveRealtime(admin, { accounting_state: "settled" }, response())).toBe(false);
  });

  it("resolves an unknown response from the request page with its evidence", async () => {
    const posts = serve(detail());
    render(page());
    const button = await screen.findByRole("button", { name: "Resolve response 2" });
    expect(screen.queryByRole("button", { name: "Resolve response 1" })).toBeNull();
    await userEvent.click(button);
    const dialog = await screen.findByRole("dialog");
    expect(dialog.textContent).toMatch(/pinned price/);
    for (const [label, value] of [["Text input tokens", "10"], ["Cached text input tokens", "4"], ["Audio input tokens", "20"], ["Cached audio input tokens", "5"], ["Text output tokens", "10"], ["Audio output tokens", "9007199254740993"], ["Authoritative evidence reference", "invoice:7"]] as const) {
      await userEvent.type(screen.getByRole("textbox", { name: new RegExp(`^${label}`) }), value);
    }
    await userEvent.click(screen.getByRole("button", { name: "Resolve response" }));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0]).toEqual({ url: `/api/v1/workspaces/team/costs/${EXEC}/realtime-responses/2/reconcile`, body: { input_text_tokens: "10", cached_text_tokens: "4", input_audio_tokens: "20", cached_audio_tokens: "5", output_text_tokens: "10", output_audio_tokens: "9007199254740993", evidence: "invoice:7" } });
  });

  it("shows no resolve action to an Auditor or once the session's cost is known", async () => {
    serve(detail());
    render(page(auditor));
    await screen.findByText("Response 2");
    expect(screen.queryByRole("button", { name: /Resolve response/ })).toBeNull();
    cleanup();
    serve(detail(attempt({ accounting_state: "settled", realtime_responses: [settled] })));
    render(page());
    await screen.findByText("Response 1");
    expect(screen.queryByRole("button", { name: /Resolve response/ })).toBeNull();
  });
});
