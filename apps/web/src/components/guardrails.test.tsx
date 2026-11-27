// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { DashboardNavigationProvider } from "./navigation-link";
import { AdminGuardrails, WorkspaceGuardrailsAdmin, WorkspaceGuardrailsTab } from "../pages/guardrails";
import { GuardrailDecisions } from "../pages/request-detail";
import {
  guardrailsPath, inspectionSentence, platformGuardrailsPath, ruleInput, stricterActions, toInput, typeDefaultPath,
  type EffectiveEntry, type PlatformWorkspaceGuardrails, type RuleSet, type StoredRule, type TypeDefault, type WorkspaceGuardrails,
} from "../lib/guardrails";
import { attemptFilters, requestQuery } from "../lib/requests";
import { admin, auditor, markup, member, team, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function serve(routes: Record<string, (init?: RequestInit) => unknown>) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    const key = Object.keys(routes).sort((a, b) => b.length - a.length).find(r => path === r || path.startsWith(`${r}?`));
    if (key) return Promise.resolve(Response.json(routes[key]!(init)));
    return Promise.reject(new Error(`Unexpected ${init?.method ?? "GET"} ${path}`));
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}
const writes = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls
  .filter(([, init]) => ["PUT", "DELETE"].includes((init as RequestInit | undefined)?.method ?? ""))
  .map(([path, init]) => [(init as RequestInit).method, path, JSON.parse(String((init as RequestInit).body ?? "null"))]);
function mount(node: React.ReactElement) {
  render(<QueryClientProvider client={testClient()}><DashboardNavigationProvider search={{ page: "workspace-settings", ws: team.id, tab: "guardrails" }} navigate={vi.fn()}><ActionProvider>{node}</ActionProvider></DashboardNavigationProvider></QueryClientProvider>);
  return userEvent.setup();
}

const secrets: StoredRule = { id: "r-secrets", name: "Secrets", protection: "secrets", detector: { kind: "pattern", categories: ["secret"], pattern_count: 0, egress: "local" }, input: { action: "monitor", redact_scope: "user_text" }, output: { mode: "off" }, on_error: "fail_open", workloads: ["generation", "embeddings"], uninspectable: "deny", threshold: null, timeout_ms: null };
const codes: StoredRule = { ...secrets, id: "r-codes", name: "Code names", protection: "custom", detector: { kind: "pattern", categories: ["custom"], patterns: ["ACME-[0-9]{4}"], pattern_count: 1, egress: "local" }, input: { action: "block", redact_scope: "user_text" }, on_error: "fail_closed" };
const platformSet: RuleSet = { id: "set-p", layer: "type_default", revision: 3, local_only: false, rules: [secrets] };
const localSet: RuleSet = { id: "set-l", layer: "workspace_local", revision: 2, local_only: null, rules: [codes] };
const effective = (rule: StoredRule, source: EffectiveEntry["source"], set: string): EffectiveEntry => ({ ...rule, rule_set_id: set, revision: 3, source, active: true, inactive_reason: null, tightened_by: [] });
const view = (patch: Partial<WorkspaceGuardrails> = {}): WorkspaceGuardrails => ({
  workspace_id: team.id, kind: "team", enforced: true, inspected_workloads: ["generation", "embeddings"], can_edit: true,
  platform: { source: "type_default", rule_set: platformSet }, egress: { local_only: false }, local: localSet,
  effective: [effective(secrets, "type_default", "set-p"), effective(codes, "workspace_local", "set-l")], ...patch,
});

describe("Guardrails: wording and bodies (#840)", () => {
  it("says what each rule inspects, what it does and that text stays inside the gateway", () => {
    expect(inspectionSentence(secrets)).toBe("Looks for secrets (keys, tokens, private keys, connection strings) in the input of generation and embeddings requests and records what it finds; the request goes on. Checked inside the gateway; nothing is sent anywhere else.");
    expect(inspectionSentence(codes)).toContain("matches of 1 custom pattern");
    expect(inspectionSentence(codes)).toContain("refuses a request that has one");
    expect(inspectionSentence({ ...codes, workloads: ["embeddings"], detector: { ...codes.detector, patterns: undefined, pattern_count: 3 } })).toContain("3 custom patterns in the input of embeddings requests");
  });
  it("sends a stored rule back in the input shape (patterns, never counts or egress)", () => {
    expect(toInput(codes)).toEqual({ id: "r-codes", name: "Code names", protection: "custom", detector: { kind: "pattern", categories: ["custom"], patterns: ["ACME-[0-9]{4}"] }, input: { action: "block" }, on_error: "fail_closed", workloads: ["generation", "embeddings"] });
    expect(toInput({ id: "t1", tightens: "r-secrets", input: { action: "block" } })).toEqual({ id: "t1", tightens: "r-secrets", input: { action: "block" } });
    expect(ruleInput({ name: " Tickets ", protection: "custom", action: "monitor", patterns: "TICKET-[0-9]+\n\n  JIRA-[A-Z]+ ", workloads: '["generation"]' })).toEqual({ name: "Tickets", protection: "custom", detector: { kind: "pattern", categories: ["custom"], patterns: ["TICKET-[0-9]+", "JIRA-[A-Z]+"] }, input: { action: "monitor" }, workloads: ["generation"] });
    expect(ruleInput({ name: "Keys", protection: "secrets", action: "block", patterns: "ignored", workloads: '["generation","embeddings"]' }, "r-1")).toEqual({ id: "r-1", name: "Keys", protection: "secrets", detector: { kind: "pattern", categories: ["secret"] }, input: { action: "block" }, workloads: ["generation", "embeddings"] });
  });
  it("offers only stricter actions when tightening", () => {
    expect(stricterActions("monitor")).toEqual(["monitor", "block"]);
    expect(stricterActions("block")).toEqual(["block"]);
  });
});

describe("Workspace › Settings › Guardrails", () => {
  it("shows the rules in effect with provenance, and the workspace's own rules to its admins", () => {
    const html = markup(<WorkspaceGuardrailsTab workspace={team} />, [[guardrailsPath(team.id), view()]]);
    expect(html).toContain("Rules in effect");
    expect(html).toContain("Workspace-type default");
    expect(html).toContain("This workspace");
    expect(html).toContain("Checked inside the gateway");
    expect(html).toContain("Tighten Secrets");
    expect(html).toContain("This workspace&#x27;s rules");
    expect(html).toContain("Add rule");
    // Other workloads and response text are said to be unchecked.
    expect(html).toContain("Other workloads are not checked yet");
  });
  it("shows members the rules and a count of patterns, never an editor", () => {
    const counted: StoredRule = { ...codes, detector: { kind: "pattern", categories: ["custom"], pattern_count: 1, egress: "local" } };
    const html = markup(<WorkspaceGuardrailsTab workspace={member} />, [[guardrailsPath(member.id), view({ can_edit: false, local: { ...localSet, rules: [counted] }, effective: [effective(secrets, "type_default", "set-p"), effective(counted, "workspace_local", "set-l")] })]]);
    expect(html).toContain("1 custom pattern");
    expect(html).not.toContain("ACME-");
    expect(html).not.toContain("Add rule");
    expect(html).not.toContain("Tighten");
  });
  it("tightens a platform rule as a local entry and keeps the layer's other entries", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: init => init?.method === "PUT" ? view() : view() });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Tighten Secrets" }));
    await user.click(screen.getByRole("button", { name: "Tighten" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]).toEqual(["PUT", guardrailsPath(team.id), { rules: [toInput(codes), { tightens: "r-secrets", input: { action: "block" } }] }]);
  });
  it("adds a custom-pattern rule to the workspace's layer", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: () => view() });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Add rule" }));
    await user.type(screen.getByLabelText("Name"), "Tickets");
    await user.selectOptions(screen.getByLabelText("Looks for"), "custom");
    await user.click(screen.getByLabelText("Patterns"));
    await user.paste("TICKET-[0-9]{4}");
    await user.click(screen.getByRole("button", { name: "Add rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const [, , body] = writes(fetch)[0]!;
    expect(body.rules).toEqual([toInput(codes), { name: "Tickets", protection: "custom", detector: { kind: "pattern", categories: ["custom"], patterns: ["TICKET-[0-9]{4}"] }, input: { action: "monitor" }, workloads: ["generation", "embeddings"] }]);
  });
});

const platformView = (patch: Partial<PlatformWorkspaceGuardrails> = {}): PlatformWorkspaceGuardrails => ({
  workspace_id: team.id, kind: "team", enforced: true, inspected_workloads: ["generation", "embeddings"], platform: { source: "type_default", rule_set: platformSet }, egress: { local_only: false },
  type_default: platformSet, override: null, local: localSet, local_visible: true, effective: view().effective, decisions_this_month: [{ action: "blocked", category: "custom", decisions: 4, findings: 5 }], ...patch,
});

describe("Admin › Guardrails", () => {
  it("shows a type default to Auditors without an editor", () => {
    const data: TypeDefault = { kind: "team", rule_set: platformSet, inspected_workloads: ["generation", "embeddings"] };
    const html = markup(<AdminGuardrails session={auditor} tab="team" onTabChange={() => {}} />, [[typeDefaultPath("team"), data]]);
    expect(html).toContain("Team default");
    expect(html).toContain("Secrets");
    expect(html).not.toContain("Add rule");
    const write = markup(<AdminGuardrails session={admin} tab="team" onTabChange={() => {}} />, [[typeDefaultPath("team"), data]]);
    expect(write).toContain("Add rule");
  });
  it("shows a workspace's totals and creates an override from the default without its ids", async () => {
    const fetch = serve({ [platformGuardrailsPath(team.id)]: () => platformView() });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    expect(await screen.findByText("Decisions this month")).toBeTruthy();
    expect(screen.getByText("Blocked")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Override the default" }));
    await user.click(screen.getByRole("button", { name: "Create override" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const { id: _id, ...withoutId } = toInput(secrets);
    expect(writes(fetch)[0]).toEqual(["PUT", platformGuardrailsPath(team.id), { local_only: false, rules: [withoutId] }]);
  });
  it("resets an override with a DELETE", async () => {
    const fetch = serve({ [platformGuardrailsPath(team.id)]: init => init?.method === "DELETE" ? platformView() : platformView({ override: { ...platformSet, id: "set-o", layer: "workspace_override" }, platform: { source: "workspace_override", rule_set: platformSet } }) });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    await user.click(await screen.findByRole("button", { name: "Reset to default" }));
    await user.click(screen.getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(writes(fetch)).toEqual([["DELETE", platformGuardrailsPath(team.id), null]]));
  });
});

describe("Logs: guardrail filters and decisions", () => {
  it("sends the guardrail filter on Requests and never to the attempt tabs", () => {
    const filters = { guardrail: "refused" as const, status: "blocked,failed" };
    expect(requestQuery(filters).query.toString()).toBe("status=blocked%2Cfailed&guardrail=refused");
    expect(requestQuery(attemptFilters(filters)).query.toString()).toBe("status=failed");
  });
  it("lists a request's decisions without any matched text", () => {
    const html = markup(<GuardrailDecisions decisions={[{ rule_set_id: "set-p", rule_id: "11111111-2222-4333-8444-555555555555", revision: 3, layer: "workspace_local", stage: "input", detector: "pattern", category: "custom", action: "blocked", count: 2, refused: true, created_at: "2026-11-26T00:00:00Z" }]} />);
    expect(html).toContain("Blocked");
    expect(html).toContain("Refused the request");
    expect(html).toContain("Custom patterns");
    expect(html).toContain("Workspace&#x27;s own rule");
  });
});
