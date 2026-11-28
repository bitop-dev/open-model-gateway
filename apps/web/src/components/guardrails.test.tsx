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
  guardrailsPath, inspectedSentence, inspectionSentence, platformGuardrailsPath, refusesWhole, ruleInput, stricterActions, stricterPart, toInput, typeDefaultPath,
  type EffectiveEntry, type Kind, type Workload, type PlatformWorkspaceGuardrails, type RuleSet, type StoredRule, type TypeDefault, type WorkspaceGuardrails,
} from "../lib/guardrails";
import { attemptFilters, logsSearch, requestFilters, requestQuery } from "../lib/requests";
import { dashboardHref, parseDashboardLocation } from "../lib/locations";
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
    if (key) { const out = routes[key]!(init); return Promise.resolve(out instanceof Response ? out : Response.json(out)); }
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
    expect(inspectionSentence(secrets)).toBe("Looks for secrets (keys, tokens, private keys, connection strings) in the input of generation and embeddings requests and records what it finds; the request goes on. If the rule can't run, the request goes on and that is recorded. Checked inside the gateway; nothing is sent anywhere else.");
    expect(inspectionSentence(codes)).toContain("matches of 1 custom pattern");
    expect(inspectionSentence(codes)).toContain("refuses a request that has one");
    expect(inspectionSentence(codes)).toContain("If the rule can't run, the request is refused.");
    // A Block rule also refuses what it can't read.
    expect(inspectionSentence(codes)).toContain("It also refuses a request it can't read (one with an image, audio or a document that isn't plain text), and a native batch when it is created.");
    expect(inspectionSentence(secrets)).not.toContain("can't read");
    expect(inspectedSentence(["generation", "embeddings"])).toContain("the request is refused with a 503 under any rule, Monitor included");
    expect(inspectionSentence({ ...secrets, input: { action: "off" } })).toBe("Off: checks nothing on input.");
    expect(inspectionSentence({ ...codes, workloads: ["embeddings"], detector: { ...codes.detector, patterns: undefined, pattern_count: 3 } })).toContain("3 custom patterns in the input of embeddings requests");
  });
  it("sends a stored entry back with every stored setting (patterns, never counts or egress)", () => {
    expect(toInput(codes)).toEqual({ id: "r-codes", name: "Code names", protection: "custom", detector: { kind: "pattern", categories: ["custom"], patterns: ["ACME-[0-9]{4}"] }, input: { action: "block", redact_scope: "user_text" }, output: { mode: "off" }, on_error: "fail_closed", workloads: ["generation", "embeddings"], uninspectable: "deny" });
    expect(toInput({ id: "t1", tightens: "r-secrets", input: { action: "block" } })).toEqual({ id: "t1", tightens: "r-secrets", input: { action: "block" } });
    expect(toInput({ id: "t2", tightens: "r-secrets", output: { mode: "off" }, on_error: "fail_closed" })).toEqual({ id: "t2", tightens: "r-secrets", output: { mode: "off" }, on_error: "fail_closed" });
    expect(ruleInput({ name: " Tickets ", protection: "custom", action: "monitor", patterns: "TICKET-[0-9]+\n\n  JIRA-[A-Z]+ ", workloads: '["generation"]' })).toEqual({ name: "Tickets", protection: "custom", detector: { kind: "pattern", categories: ["custom"], patterns: ["TICKET-[0-9]+", "JIRA-[A-Z]+"] }, input: { action: "monitor" }, workloads: ["generation"] });
    // An edit starts from the stored rule: what the dialog doesn't show keeps its value.
    expect(ruleInput({ name: "Keys", protection: "secrets", action: "block", on_error: "fail_open", patterns: "ignored", workloads: '["generation","embeddings"]' }, toInput(secrets))).toEqual({ id: "r-secrets", name: "Keys", protection: "secrets", detector: { kind: "pattern", categories: ["secret"] }, input: { action: "block", redact_scope: "user_text" }, output: { mode: "off" }, on_error: "fail_open", workloads: ["generation", "embeddings"], uninspectable: "deny" });
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
    expect(html).toContain("A rule may also name image generation, transcription and realtime, whose content is never read");
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
    const fetch = serve({ [guardrailsPath(team.id)]: () => view() });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Tighten Secrets" }));
    // Only a stricter action: on_error covers a rule that can't run, not a busy gateway.
    expect(screen.queryByRole("switch")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Tighten" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]).toEqual(["PUT", guardrailsPath(team.id), { expected: { rule_set_id: "set-l", revision: 2 }, rules: [toInput(codes), { tightens: "r-secrets", input: { action: "block" } }] }]);
  });
  it("tightening again keeps the existing tightening's other settings", async () => {
    const tightening = { id: "t-1", tightens: "r-secrets", on_error: "fail_closed" as const };
    const tightened = { ...effective(secrets, "type_default", "set-p"), on_error: "fail_closed" as const, tightened_by: ["t-1"] };
    const fetch = serve({ [guardrailsPath(team.id)]: () => view({ local: { ...localSet, rules: [codes, tightening] }, effective: [tightened, effective(codes, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Tighten Secrets" }));
    await user.click(screen.getByRole("button", { name: "Tighten" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]![2].rules).toEqual([toInput(codes), { id: "t-1", tightens: "r-secrets", input: { action: "block" }, on_error: "fail_closed" }]);
  });
  it("edits a rule without changing the settings the dialog leaves alone", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: () => view() });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Edit Code names" }));
    // Block's own default: shown as the default, so a change of action takes the new action's.
    expect((screen.getByLabelText("If the rule can't run") as HTMLSelectElement).value).toBe("");
    expect(screen.getByText(/A request is never let through unchecked/)).toBeTruthy();
    await user.clear(screen.getByLabelText("Name"));
    await user.type(screen.getByLabelText("Name"), "Project codes");
    await user.click(screen.getByRole("button", { name: "Save rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const { on_error: _default, ...stored } = toInput(codes);
    expect(writes(fetch)[0]![2].rules).toEqual([{ ...stored, name: "Project codes" }]);
  });
  it("renaming a rule that looks for secrets and custom patterns sends its detector back unchanged", async () => {
    const both: StoredRule = { ...codes, id: "r-both", name: "Both", protection: "secrets", detector: { kind: "pattern", categories: ["secret", "custom"], patterns: ["ACME-[0-9]{4}"], pattern_count: 1, egress: "local" } };
    expect(inspectionSentence(both)).toContain("Looks for secrets (keys, tokens, private keys, connection strings) and matches of 1 custom pattern");
    const fetch = serve({ [guardrailsPath(team.id)]: () => view({ local: { ...localSet, rules: [both] }, effective: [effective(both, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    expect(await screen.findByText(/^Secrets and custom patterns · /)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Edit Both" }));
    expect((screen.getByLabelText("Looks for") as HTMLSelectElement).value).toBe("both");
    await user.type(screen.getByLabelText("Name"), " renamed");
    await user.click(screen.getByRole("button", { name: "Save rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const sent = writes(fetch)[0]![2].rules[0];
    expect(sent.detector).toEqual(toInput(both).detector);
    expect(sent.protection).toBe("secrets");
    expect(sent.name).toBe("Both renamed");
    // A pattern stored through the API with a leading space is kept as stored.
    const spaced = { ...toInput({ ...codes, detector: { ...codes.detector, patterns: [" ACME-[0-9]{4}"] } }) };
    expect((ruleInput({ name: "Code names", protection: "custom", action: "block", patterns: "ACME-[0-9]{4}", workloads: '["generation","embeddings"]' }, spaced).detector as { patterns: string[] }).patterns).toEqual([" ACME-[0-9]{4}"]);
  });
  it("shows a chosen on_error that is not its action's default", async () => {
    const strict: StoredRule = { ...secrets, on_error: "fail_closed" };
    serve({ [guardrailsPath(team.id)]: () => view({ local: { ...localSet, rules: [strict] }, effective: [effective(strict, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Edit Secrets" }));
    expect((screen.getByLabelText("If the rule can't run") as HTMLSelectElement).value).toBe("fail_closed");
  });
  it("counts the layer's other patterns against its limit", async () => {
    const many: StoredRule = { ...codes, id: "r-many", name: "Many", detector: { ...codes.detector, patterns: Array.from({ length: 49 }, (_, i) => `P${i}-[0-9]`), pattern_count: 49 } };
    const fetch = serve({ [guardrailsPath(team.id)]: () => view({ local: { ...localSet, rules: [many] }, effective: [effective(many, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Add rule" }));
    await user.type(screen.getByLabelText("Name"), "Two more");
    await user.selectOptions(screen.getByLabelText("Looks for"), "custom");
    await user.click(screen.getByLabelText("Patterns"));
    await user.paste("A-[0-9]\nB-[0-9]");
    await user.click(screen.getByRole("button", { name: "Add rule" }));
    expect(await screen.findByText("This layer allows 50 patterns in all; 1 are left for this rule.")).toBeTruthy();
    expect(writes(fetch)).toHaveLength(0);
  });
  it("leaves out a tightening whose platform rule is gone when the layer is saved", async () => {
    const old = { id: "t-old", tightens: "r-gone", input: { action: "block" as const } };
    const dangling: EffectiveEntry = { id: "t-old", tightens: "r-gone", rule_set_id: "set-l", revision: 2, source: "workspace_local", active: false, inactive_reason: "tightened_rule_missing", tightened_by: [] };
    const fetch = serve({ [guardrailsPath(team.id)]: () => view({ local: { ...localSet, rules: [codes, old] }, effective: [effective(secrets, "type_default", "set-p"), effective(codes, "workspace_local", "set-l"), dangling] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    expect(await screen.findByText(/Its platform rule was removed: it no longer applies/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Remove Code names" }));
    await user.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(writes(fetch)).toEqual([["PUT", guardrailsPath(team.id), { expected: { rule_set_id: "set-l", revision: 2 }, rules: [] }]]));
  });
  it("keeps a tightening that applies again since the page loaded", async () => {
    const old = { id: "t-old", tightens: "r-secrets", input: { action: "block" as const } };
    const dangling: EffectiveEntry = { id: "t-old", tightens: "r-secrets", rule_set_id: "set-l", revision: 2, source: "workspace_local", active: false, inactive_reason: "tightened_rule_missing", tightened_by: [] };
    let loads = 0;
    // Loaded under an override without r-secrets (the tightening is gone); the override is then reset, so it applies again.
    const overridden = { ...platformSet, id: "set-o", layer: "workspace_override" as const, rules: [] };
    const fetch = serve({ [guardrailsPath(team.id)]: init => init?.method === "PUT" ? view() : loads++ === 0
      ? view({ platform: { source: "workspace_override", rule_set: overridden }, local: { ...localSet, rules: [codes, old] }, effective: [effective(codes, "workspace_local", "set-l"), dangling] })
      : view({ local: { ...localSet, rules: [codes, old] }, effective: [{ ...effective(secrets, "type_default", "set-p"), input: { action: "block" }, tightened_by: ["t-old"] }, effective(codes, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    expect(await screen.findByText(/Its platform rule was removed/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Remove Code names" }));
    await user.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(writes(fetch)).toEqual([["PUT", guardrailsPath(team.id), { expected: { rule_set_id: "set-l", revision: 2 }, rules: [old] }]]));
  });
  it("leaves out what a tightening is now weaker than, keeps what equals the platform rule, and marks it", async () => {
    // The platform raised Secrets to Block; the workspace's Monitor tightening of it (once from Off) is weaker.
    const raised: StoredRule = { ...secrets, input: { action: "block", redact_scope: "user_text" } };
    const weak = { id: "t-weak", tightens: "r-secrets", input: { action: "monitor" as const } };
    const fetch = serve({ [guardrailsPath(team.id)]: () => view({ platform: { source: "type_default", rule_set: { ...platformSet, rules: [raised] } }, local: { ...localSet, rules: [codes, weak] }, effective: [{ ...effective(raised, "type_default", "set-p"), tightened_by: ["t-weak"] }, effective(codes, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    expect(await screen.findByText(/“Secrets” is now stricter than this: it no longer applies/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Remove Code names" }));
    await user.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(writes(fetch)).toEqual([["PUT", guardrailsPath(team.id), { expected: { rule_set_id: "set-l", revision: 2 }, rules: [] }]]));
    // Equal is kept (the server accepts it, and it holds the level if the platform lowers its rule); weaker goes.
    expect(stricterPart({ id: "t", tightens: "r-secrets", input: { action: "block" }, on_error: "fail_closed" }, { ...secrets, input: { action: "block" } })).toEqual({ id: "t", tightens: "r-secrets", input: { action: "block" }, on_error: "fail_closed" });
    expect(stricterPart({ id: "t", tightens: "r-secrets", input: { action: "monitor" }, on_error: "fail_closed" }, { ...secrets, input: { action: "block" }, on_error: "fail_closed" })).toEqual({ id: "t", tightens: "r-secrets", on_error: "fail_closed" });
    expect(stricterPart({ id: "t", tightens: "r-secrets", input: { action: "monitor" } }, { ...secrets, input: { action: "block" } })).toBeNull();
  });
  it("names the platform rule when the server still refuses a tightening as loosening", async () => {
    serve({ [guardrailsPath(team.id)]: init => init?.method === "PUT"
      ? new Response(JSON.stringify({ error: { code: "403", message: "loosen", reason: "guardrail_loosen_not_allowed", rule: "11111111-2222-4333-8444-555555555555" } }), { status: 403, headers: { "content-type": "application/json" } })
      : view({ platform: { source: "type_default", rule_set: { ...platformSet, rules: [{ ...secrets, id: "11111111-2222-4333-8444-555555555555" }] } }, effective: [effective({ ...secrets, id: "11111111-2222-4333-8444-555555555555" }, "type_default", "set-p"), effective(codes, "workspace_local", "set-l")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Tighten Secrets" }));
    await user.click(screen.getByRole("button", { name: "Tighten" }));
    expect(await screen.findByText("This workspace's tightening of “Secrets” is no longer stricter than the platform rule. Remove it, then try again.")).toBeTruthy();
  });
  it("names the layer as loaded, and says so plainly when the server finds it changed", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: init => init?.method === "PUT" ? new Response(JSON.stringify({ error: { code: "409", message: "changed", reason: "guardrail_layer_changed" } }), { status: 409, headers: { "content-type": "application/json" } }) : view() });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Remove Code names" }));
    await user.click(screen.getByRole("button", { name: "Remove" }));
    expect(await screen.findByText("This changed since you opened it. The page now shows the current rules: check them and try again.")).toBeTruthy();
    expect(writes(fetch)).toEqual([["PUT", guardrailsPath(team.id), { expected: { rule_set_id: "set-l", revision: 2 }, rules: [] }]]);
    // The page reads the layer again, so the next try is built from what is stored.
    const reads = () => fetch.mock.calls.filter(([path, init]) => path === guardrailsPath(team.id) && ((init as RequestInit | undefined)?.method ?? "GET") === "GET").length;
    await waitFor(() => expect(reads()).toBeGreaterThanOrEqual(3));
  });
  it("shows an active rule that is off as Off, never Active", () => {
    const off: StoredRule = { ...secrets, input: { action: "off", redact_scope: "user_text" } };
    const html = markup(<WorkspaceGuardrailsTab workspace={team} />, [[guardrailsPath(team.id), view({ local: null, effective: [effective(off, "type_default", "set-p")] })]]);
    expect(html).not.toContain(">Active<");
    expect(html).toContain("Off: checks nothing on input.");
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
    const local = { ...localSet, rules: [codes, { id: "t-1", tightens: "r-secrets", input: { action: "block" as const } }] };
    const fetch = serve({ [platformGuardrailsPath(team.id)]: () => platformView({ local, decisions_this_month: [{ action: "blocked", category: "custom", decisions: 4, findings: 5 }, { action: "detector_failed", category: "custom", decisions: 1, findings: 0 }] }) });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    expect(await screen.findByText("Decisions this month")).toBeTruthy();
    expect(screen.getByText("Blocked")).toBeTruthy();
    expect(screen.getByRole("table", { name: "Guardrail decisions this month" }).textContent).toContain("—");
    await user.click(screen.getByRole("button", { name: "Override the default" }));
    // A tightening of a default rule stops applying under the override's new rules.
    expect(screen.getByRole("dialog").textContent).toContain("This workspace's tightening of the default's rules stops applying");
    await user.click(screen.getByRole("button", { name: "Create override" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const { id: _id, ...withoutId } = toInput(secrets);
    expect(writes(fetch)[0]).toEqual(["PUT", platformGuardrailsPath(team.id), { local_only: false, expected: null, rules: [withoutId] }]);
  });
  it("resets an override with a DELETE", async () => {
    const fetch = serve({ [platformGuardrailsPath(team.id)]: init => init?.method === "DELETE" ? platformView() : platformView({ override: { ...platformSet, id: "set-o", layer: "workspace_override" }, platform: { source: "workspace_override", rule_set: platformSet } }) });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    await user.click(await screen.findByRole("button", { name: "Reset to default" }));
    await user.click(screen.getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(writes(fetch)).toEqual([["DELETE", `${platformGuardrailsPath(team.id)}?rule_set_id=set-o&revision=3`, null]]));
  });
});

describe("Admin › Guardrails: reach and fresh reads", () => {
  it("says a type default's edit reaches every workspace of the kind", async () => {
    const data: TypeDefault = { kind: "team", rule_set: platformSet, inspected_workloads: ["generation", "embeddings"] };
    serve({ [typeDefaultPath("team")]: () => data });
    const user = mount(<AdminGuardrails session={admin} tab="team" onTabChange={() => {}} />);
    await user.click(await screen.findByRole("button", { name: "Remove Secrets" }));
    expect(screen.getByRole("dialog").textContent).toContain("It stops applying to every team workspace without a platform override, and their tightenings of it stop applying.");
  });
  it("resets the override as loaded, and says so plainly when the server finds it changed", async () => {
    const overridden = platformView({ override: { ...platformSet, id: "set-o", layer: "workspace_override", revision: 1 }, platform: { source: "workspace_override", rule_set: platformSet } });
    const fetch = serve({ [platformGuardrailsPath(team.id)]: init => init?.method === "DELETE" ? new Response(JSON.stringify({ error: { code: "409", message: "changed", reason: "guardrail_layer_changed" } }), { status: 409, headers: { "content-type": "application/json" } }) : overridden });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    await user.click(await screen.findByRole("button", { name: "Reset to default" }));
    await user.click(screen.getByRole("button", { name: "Reset" }));
    expect(await screen.findByText("This changed since you opened it. The page now shows the current rules: check them and try again.")).toBeTruthy();
    expect(writes(fetch)).toEqual([["DELETE", `${platformGuardrailsPath(team.id)}?rule_set_id=set-o&revision=1`, null]]);
  });
  it("creates an override from the default as it is when the override is created", async () => {
    let loads = 0;
    const later: StoredRule = { ...secrets, id: "r-later", name: "Added since" };
    const fetch = serve({ [platformGuardrailsPath(team.id)]: init => init?.method === "PUT" ? platformView() : loads++ === 0 ? platformView() : platformView({ type_default: { ...platformSet, revision: 4, local_only: true, rules: [secrets, later] } }) });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    await user.click(await screen.findByRole("button", { name: "Override the default" }));
    await user.click(screen.getByRole("button", { name: "Create override" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const strip = (r: StoredRule) => { const { id: _id, ...rest } = toInput(r); return rest; };
    expect(writes(fetch)[0]![2]).toEqual({ local_only: true, expected: null, rules: [strip(secrets), strip(later)] });
  });
});

describe("Admin › Guardrails: every platform write names the layer as loaded", () => {
  it("edits the type default and its local_only from the layer the page loaded", async () => {
    const data: TypeDefault = { kind: "team", rule_set: platformSet, inspected_workloads: ["generation", "embeddings"] };
    const fetch = serve({ [typeDefaultPath("team")]: () => data });
    const user = mount(<AdminGuardrails session={admin} tab="team" onTabChange={() => {}} />);
    await user.click(await screen.findByRole("button", { name: "Remove Secrets" }));
    await user.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]).toEqual(["PUT", typeDefaultPath("team"), { local_only: false, expected: { rule_set_id: "set-p", revision: 3 }, rules: [] }]);
    await user.click(await screen.findByRole("button", { name: /^Text stays inside/ }));
    await user.click(screen.getByRole("switch"));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(2));
    expect(writes(fetch)[1]).toEqual(["PUT", typeDefaultPath("team"), { local_only: true, expected: { rule_set_id: "set-p", revision: 3 }, rules: [toInput(secrets)] }]);
  });
  it("edits an override and its local_only from the override the page loaded", async () => {
    const override = { ...platformSet, id: "set-o", layer: "workspace_override" as const, revision: 5 };
    const fetch = serve({ [platformGuardrailsPath(team.id)]: () => platformView({ override, platform: { source: "workspace_override", rule_set: override } }) });
    const user = mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    await user.click(await screen.findByRole("button", { name: "Remove Secrets" }));
    await user.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]).toEqual(["PUT", platformGuardrailsPath(team.id), { local_only: false, expected: { rule_set_id: "set-o", revision: 5 }, rules: [] }]);
    await user.click(await screen.findByRole("button", { name: /^Text stays inside/ }));
    await user.click(screen.getByRole("switch"));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(2));
    expect(writes(fetch)[1]).toEqual(["PUT", platformGuardrailsPath(team.id), { local_only: true, expected: { rule_set_id: "set-o", revision: 5 }, rules: [toInput(secrets)] }]);
  });
});

describe("Personal data (#841)", () => {
  const personal: StoredRule = { ...secrets, id: "r-pd", name: "Personal data", protection: "personal_data", detector: { kind: "pattern", categories: ["personal_data"], kinds: ["email", "ip"], pattern_count: 0, egress: "local" }, input: { action: "block", redact_scope: "user_text" }, on_error: "fail_closed" };
  it("says which kinds a rule looks for, and sends them back", () => {
    expect(inspectionSentence(personal)).toContain("Looks for personal data: email addresses and public IP addresses in the input");
    // A rule written through the API with another category too says both.
    expect(inspectionSentence({ ...personal, detector: { ...personal.detector, categories: ["personal_data", "secret"] } })).toContain("personal data: email addresses and public IP addresses and secrets (keys");
    expect(toInput(personal)).toMatchObject({ protection: "personal_data", detector: { kind: "pattern", categories: ["personal_data"], kinds: ["email", "ip"] } });
  });
  it("adds a personal-data rule with only the kinds ticked, none to begin with", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: () => view() });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Add rule" }));
    await user.type(screen.getByLabelText("Name"), "No cards");
    await user.selectOptions(screen.getByLabelText("Looks for"), "personal");
    const boxes = screen.getAllByRole("checkbox").filter(b => /^(Email|Phone|Payment card|IBAN|US SSN|IP address):/.test(b.getAttribute("aria-label") ?? b.closest("label")?.textContent ?? ""));
    expect(boxes).toHaveLength(6);
    expect(boxes.every(b => b.getAttribute("aria-checked") === "false")).toBe(true);
    expect(screen.getByText(/Public IP addresses .*only: never loopback/)).toBeTruthy();
    // Nothing ticked: nothing to look for, so the dialog refuses.
    await user.click(screen.getByRole("button", { name: "Add rule" }));
    expect(writes(fetch)).toHaveLength(0);
    await user.click(screen.getByRole("checkbox", { name: /^Payment card:/ }));
    await user.selectOptions(screen.getByLabelText("On input"), "block");
    await user.click(screen.getByRole("button", { name: "Add rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]![2].rules[1]).toEqual({ name: "No cards", protection: "personal_data", detector: { kind: "pattern", categories: ["personal_data"], kinds: ["card"] }, input: { action: "block" }, workloads: ["generation", "embeddings"] });
  });
  it("shows a decision's kind", () => {
    const html = markup(<GuardrailDecisions decisions={[{ rule_set_id: "set-p", rule_id: "11111111-2222-4333-8444-555555555555", revision: 3, layer: "type_default", stage: "input", detector: "pattern", category: "personal_data", kind: "card", action: "blocked", count: 1, refused: true, created_at: "2026-11-26T00:00:00Z" }]} />);
    expect(html).toContain("Personal data · Payment card");
  });
  it("names a kind this dashboard does not know by its value, never \"undefined\"", async () => {
    const newer = { ...personal, detector: { ...personal.detector, kinds: ["email", "passport"] as Kind[] } };
    expect(inspectionSentence(newer)).toContain("personal data: email addresses and passport in the input");
    const html = markup(<GuardrailDecisions decisions={[{ rule_set_id: "set-p", rule_id: "11111111-2222-4333-8444-555555555555", revision: 3, layer: "type_default", stage: "input", detector: "pattern", category: "personal_data", kind: "passport", action: "monitored", count: 1, refused: false, created_at: "2026-11-26T00:00:00Z" }]} />);
    expect(html).toContain("Personal data · passport");
    serve({ [platformGuardrailsPath(team.id)]: () => platformView({ decisions_this_month: [{ action: "monitored", category: "personal_data", kind: "passport" as Kind, decisions: 2, findings: 3 }] }) });
    mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    const table = await screen.findByRole("table", { name: "Guardrail decisions this month" });
    expect(table.textContent).toContain("passport");
    expect(table.textContent).not.toContain("undefined");
  });
  const local = (rule: StoredRule) => view({ local: { ...localSet, rules: [rule] }, effective: [effective(rule, "workspace_local", "set-l")] });
  it("renames a personal-data rule and changes its kinds", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: () => local(personal) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Edit Personal data" }));
    expect((screen.getByLabelText("Looks for") as HTMLSelectElement).value).toBe("personal");
    expect(screen.getByRole("checkbox", { name: /^Email:/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("checkbox", { name: /^IP address:/ }).getAttribute("aria-checked")).toBe("true");
    await user.clear(screen.getByLabelText("Name"));
    await user.type(screen.getByLabelText("Name"), "No cards or mail");
    await user.click(screen.getByRole("checkbox", { name: /^IP address:/ }));
    await user.click(screen.getByRole("checkbox", { name: /^Payment card:/ }));
    await user.click(screen.getByRole("button", { name: "Save rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const sent = writes(fetch)[0]![2].rules[0];
    expect(sent).toMatchObject({ id: "r-pd", name: "No cards or mail", protection: "personal_data", input: { action: "block" } });
    expect(sent.detector.categories).toEqual(["personal_data"]);
    expect([...sent.detector.kinds].sort()).toEqual(["card", "email"]);
  });
  it("turns a personal-data rule into a secrets rule without its kinds", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: () => local(personal) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Edit Personal data" }));
    await user.selectOptions(screen.getByLabelText("Looks for"), "secrets");
    await user.click(screen.getByRole("button", { name: "Save rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const sent = writes(fetch)[0]![2].rules[0];
    expect(sent.protection).toBe("secrets");
    expect(sent.detector).toEqual({ kind: "pattern", categories: ["secret"] });
  });
  it("turns a secrets rule into a personal-data rule only with a kind ticked", async () => {
    const fetch = serve({ [guardrailsPath(team.id)]: () => local(secrets) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Edit Secrets" }));
    await user.selectOptions(screen.getByLabelText("Looks for"), "personal");
    expect(screen.getAllByRole("checkbox").filter(b => /^(Email|Phone|Payment card|IBAN|US SSN|IP address):/.test(b.getAttribute("aria-label") ?? b.closest("label")?.textContent ?? "")).every(b => b.getAttribute("aria-checked") === "false")).toBe(true);
    await user.click(screen.getByRole("button", { name: "Save rule" }));
    expect(writes(fetch)).toHaveLength(0);
    await user.click(screen.getByRole("checkbox", { name: /^Phone:/ }));
    await user.click(screen.getByRole("button", { name: "Save rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const sent = writes(fetch)[0]![2].rules[0];
    expect(sent.protection).toBe("personal_data");
    expect(sent.detector).toEqual({ kind: "pattern", categories: ["personal_data"], kinds: ["phone"] });
  });
});

describe("Logs: guardrail filters and decisions", () => {
  it("keeps the guardrail filter in the URL and across the log tabs", () => {
    const search = { page: "requests" as const, ws: team.id, status: "blocked", guardrail: "refused" as const };
    const href = dashboardHref(search);
    expect(href).toBe(`/workspaces/${team.id}/logs?status=blocked&guardrail=refused`);
    expect(parseDashboardLocation(href)).toMatchObject(search);
    const scope = { kind: "workspace" as const, workspace: team };
    const filters = requestFilters(search);
    // Requests → Generations → Requests keeps both, though Generations never sends them.
    const generations = logsSearch(scope, filters, "generations");
    expect(generations).toMatchObject({ tab: "generations", guardrail: "refused", status: "blocked" });
    expect(logsSearch(scope, requestFilters(generations), "requests")).toMatchObject({ guardrail: "refused", status: "blocked" });
    expect(requestQuery(attemptFilters(requestFilters(generations))).query.toString()).toBe("");
  });
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

describe("What a rule can't read (#841)", () => {
  const images: StoredRule = { ...codes, id: "r-img", name: "No images", workloads: ["images"], uninspectable: "deny" };
  it("says a Block rule naming a never-read workload refuses every request of it, or lets them through under allow", () => {
    expect(inspectionSentence(images)).toBe("Image generation requests are never read: it refuses every one. If the rule can't run, the request is refused. Checked inside the gateway; nothing is sent anywhere else.");
    expect(inspectionSentence({ ...images, uninspectable: "allow" })).toContain("Image generation requests are never read: it lets them through unread and records each as not readable.");
    const both = { ...codes, workloads: ["generation", "realtime"] as Workload[], uninspectable: "allow" as const };
    expect(inspectionSentence(both)).toContain("in the input of generation requests and refuses a request that has one. A request it can't read (one with an image, audio or a document that isn't plain text) goes on with its text checked, and is recorded as not readable");
    expect(inspectionSentence(both)).toContain("Realtime requests are never read: it lets them through unread");
    expect(refusesWhole({ action: "block", workloads: ["images", "realtime"] }, "every team workspace")).toBe("This refuses all image generation and realtime requests for every team workspace: their content is never read, so a Block rule that refuses what it can't read refuses every one.");
    expect(refusesWhole({ action: "block", uninspectable: "allow", workloads: ["images"] }, "x")).toBeUndefined();
    expect(refusesWhole({ action: "monitor", workloads: ["images"] }, "x")).toBeUndefined();
  });
  it("warns before a type default's Block rule switches image generation off for every workspace of the kind", async () => {
    const data: TypeDefault = { kind: "team", rule_set: platformSet, inspected_workloads: ["generation", "embeddings"] };
    const fetch = serve({ [typeDefaultPath("team")]: () => data });
    const user = mount(<AdminGuardrails session={admin} tab="team" onTabChange={() => {}} />);
    await user.click(await screen.findByRole("button", { name: "Add rule" }));
    await user.type(screen.getByLabelText("Name"), "No images");
    await user.selectOptions(screen.getByLabelText("On input"), "block");
    await user.click(screen.getByRole("checkbox", { name: /^Image generation/ }));
    expect(screen.getByRole("dialog").textContent).toContain("This refuses all image generation requests for every team workspace without a platform override");
    // Letting what it can't read through: no warning, and the choice is sent.
    await user.selectOptions(screen.getByLabelText("What it can't read"), "allow");
    expect(screen.getByRole("dialog").textContent).not.toContain("This refuses all image generation requests");
    await user.click(screen.getByRole("button", { name: "Add rule" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]![2].rules[1]).toMatchObject({ name: "No images", input: { action: "block" }, uninspectable: "allow", workloads: ["generation", "embeddings", "images"] });
  });
  it("tightens a platform rule that lets unreadable content through to refuse it", async () => {
    const lets: StoredRule = { ...codes, id: "r-lets", name: "Lets through", uninspectable: "allow" };
    const fetch = serve({ [guardrailsPath(team.id)]: () => view({ platform: { source: "type_default", rule_set: { ...platformSet, rules: [lets] } }, local: { ...localSet, rules: [] }, effective: [effective(lets, "type_default", "set-p")] }) });
    const user = mount(<WorkspaceGuardrailsTab workspace={team} />);
    await user.click(await screen.findByRole("button", { name: "Tighten Lets through" }));
    expect((screen.getByLabelText("What it can't read") as HTMLSelectElement).value).toBe("deny");
    await user.click(screen.getByRole("button", { name: "Tighten" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]![2].rules).toEqual([{ tightens: "r-lets", uninspectable: "deny" }]);
    // A tightening keeps deny against its platform rule; allow is never sent.
    expect(stricterPart({ tightens: "r-lets", uninspectable: "deny" }, lets)).toEqual({ tightens: "r-lets", uninspectable: "deny" });
    expect(stricterPart({ tightens: "r-lets", uninspectable: "allow" }, lets)).toBeNull();
  });
  it("counts not readable requests refused apart from those let through, and the request page says which", async () => {
    serve({ [platformGuardrailsPath(team.id)]: () => platformView({ decisions_this_month: [{ action: "skipped", category: "secret", refused: true, decisions: 2, findings: 0 }, { action: "skipped", category: "secret", refused: false, decisions: 5, findings: 0 }] }) });
    mount(<WorkspaceGuardrailsAdmin id={team.id} writable />);
    const table = await screen.findByRole("table", { name: "Guardrail decisions this month" });
    expect(table.textContent).toContain("Not readable · refused");
    expect(table.textContent).toContain("Not readable · let through");
    expect(screen.getByText(/refused ones \(the rule refuses what it can't read\) apart from ones let through unread/)).toBeTruthy();
    const html = markup(<GuardrailDecisions decisions={[{ rule_set_id: "set-p", rule_id: "11111111-2222-4333-8444-555555555555", revision: 3, layer: "type_default", stage: "input", detector: "pattern", category: "secret", kind: null, action: "skipped", count: 1, refused: false, created_at: "2026-11-26T00:00:00Z" }]} />);
    expect(html).toContain("Let through unread");
  });
});
