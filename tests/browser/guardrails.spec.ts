import { expect, test } from "@playwright/test";
import { BASE_URL } from "./stack.mjs";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, readFixture, signIn } from "./helpers";

/*
 * Guardrails dashboard (#840): a Team admin adds a blocking custom-pattern rule in
 * Workspace › Settings › Guardrails, a member sees the rule without its pattern,
 * a request that matches it is refused before any attempt and shows as refused in
 * Logs with its decision, and a Platform Admin overrides and resets the Team's
 * default while an Auditor reads the type defaults without actions. The pattern
 * and the matching text are synthetic; matched text is never shown anywhere.
 */
test.afterEach(expectNoCspViolations);
test.use({ trace: "off", screenshot: "off", video: "off" });

test("a workspace rule blocks a request and the admin pages show the layers", async ({ browser }) => {
  const ws = readFixture().product!, unique = Date.now().toString(36);
  const rule = `Ticket codes ${unique}`, local = `/workspaces/${ws}/guardrails`, platform = `/platform/workspaces/${ws}/guardrails`;
  const alex = await signIn(browser, "alex"), blair = await signIn(browser, "blair"), operator = await signIn(browser, "operator"), auditor = await signIn(browser, "auditor");
  try {
    // A Team admin adds a blocking custom-pattern rule.
    await alex.page.goto(`/workspaces/${ws}/settings?tab=guardrails`);
    await expect(alex.page.getByText("What is checked")).toBeVisible();
    await alex.page.getByRole("button", { name: "Add rule" }).click();
    const add = alex.page.getByRole("dialog", { name: "Add a rule to this workspace's rules" });
    await add.getByRole("textbox", { name: "Name" }).fill(rule);
    await add.getByRole("combobox", { name: "Looks for" }).selectOption("custom");
    await add.getByRole("textbox", { name: "Patterns" }).fill("TICKET-[0-9]{4}");
    await add.getByRole("combobox", { name: "On input" }).selectOption("block");
    await add.getByRole("button", { name: "Add rule" }).click();
    await expect(add).toBeHidden();
    const effective = alex.page.getByRole("table", { name: /^Guardrails in effect for / });
    await expect(effective.getByText(rule)).toBeVisible();
    await expect(effective.getByText("Looks for matches of 1 custom pattern in the input of generation and embeddings requests and refuses a request that has one.", { exact: false })).toBeVisible();
    await expectAccessible(alex.page, "workspace guardrails");
    await alex.page.setViewportSize({ width: 390, height: 844 });
    await expectNoHorizontalScroll(alex.page);

    // A member sees the rule and its pattern count, never the pattern, and cannot edit.
    await blair.page.goto(`/workspaces/${ws}/settings?tab=guardrails`);
    await expect(blair.page.getByRole("table", { name: /^Guardrails in effect for / }).getByText(rule)).toBeVisible();
    await expect(blair.page.getByText("1 custom pattern", { exact: false }).first()).toBeVisible();
    await expect(blair.page.getByText("TICKET-[0-9]{4}")).toHaveCount(0);
    await expect(blair.page.getByRole("button", { name: /^Add rule$|^Tighten |^Edit |^Remove / })).toHaveCount(0);
    expect((await api(blair.page, "PUT", local, { rules: [] })).status()).toBe(403);

    // A matching request is refused before any attempt; a clean one goes through.
    const created = await api(blair.page, "POST", `/workspaces/${ws}/keys`, { name: `guardrails-${unique}`, expires_in_days: 1, model_ids: null });
    expect(created.status()).toBe(200);
    const key = await created.json();
    const send = (content: string) => fetch(`${BASE_URL}/v1/chat/completions`, { method: "POST", headers: { authorization: `Bearer ${key.token}`, "content-type": "application/json" }, body: JSON.stringify({ model: "mock/chat", messages: [{ role: "user", content }], max_completion_tokens: 16 }) });
    const refused = await send("Please look at TICKET-4242 today");
    expect(refused.status).toBe(400);
    expect(JSON.stringify(await refused.json())).toContain("guardrail_input_blocked");
    expect((await send("Say hello")).status).toBe(200);

    await blair.page.goto(`/workspaces/${ws}/logs?status=blocked`);
    await expect(blair.page.getByText("Refused by guardrails").first()).toBeVisible();
    const requests = await (await api(blair.page, "GET", `/workspaces/${ws}/requests?status=blocked&limit=5`)).json();
    const blocked = requests.data.find((r: { key?: { id: string } }) => r.key?.id === key.id);
    expect(blocked).toMatchObject({ status: "blocked", attempts: 0, guardrail: { refused: true } });
    await blair.page.goto(`/workspaces/${ws}/requests/${blocked.root_request_id}`);
    await expect(blair.page.getByText("Refused by guardrails before any attempt: nothing was sent or charged.")).toBeVisible();
    const decisions = blair.page.getByRole("table", { name: "Guardrail decisions" });
    await expect(decisions.getByText("Blocked")).toBeVisible();
    await expect(decisions.getByText("Custom patterns")).toBeVisible();
    await expect(blair.page.getByText(/TICKET-\d/)).toHaveCount(0);
    await expectAccessible(blair.page, "request refused by guardrails");

    // A Platform Admin overrides the Team default for this workspace and resets it.
    await operator.page.goto(`/admin/teams/${ws}?tab=guardrails`);
    await expect(operator.page.getByRole("heading", { name: "Team default" })).toBeVisible();
    await expect(operator.page.getByRole("table", { name: "This workspace's rules" }).getByText(rule)).toBeVisible();
    await expect(operator.page.getByRole("table", { name: "Guardrail decisions this month" }).getByText("Blocked")).toBeVisible();
    await operator.page.getByRole("button", { name: "Override the default" }).click();
    await operator.page.getByRole("dialog", { name: "Override the default for this workspace?" }).getByRole("button", { name: "Create override" }).click();
    await expect(operator.page.getByRole("heading", { name: "Platform override" })).toBeVisible();
    expect((await (await api(operator.page, "GET", platform)).json()).override).not.toBeNull();
    await alex.page.setViewportSize({ width: 1440, height: 900 });
    await alex.page.reload();
    await expect(alex.page.getByText("Set by a Platform Admin for this workspace, with this workspace's own rules.")).toBeVisible();
    await operator.page.getByRole("button", { name: "Reset to default" }).click();
    await operator.page.getByRole("dialog", { name: "Reset to the default?" }).getByRole("button", { name: "Reset" }).click();
    await expect(operator.page.getByRole("heading", { name: "Team default" })).toBeVisible();
    expect((await (await api(operator.page, "GET", platform)).json()).override).toBeNull();
    await expectAccessible(operator.page, "admin team guardrails");

    // The type defaults: Platform Admins edit them, Auditors read them.
    await operator.page.goto("/admin/settings/guardrails");
    await expect(operator.page.getByRole("heading", { name: "Guardrails" })).toBeVisible();
    await expect(operator.page.getByRole("button", { name: "Add rule" })).toBeVisible();
    await operator.page.getByRole("tab", { name: "What runs" }).click();
    await expect(operator.page.getByRole("table", { name: "Detectors" }).getByText("Stays inside the gateway").first()).toBeVisible();
    await auditor.page.goto("/admin/settings/guardrails");
    await expect(auditor.page.getByRole("heading", { name: "Team default" })).toBeVisible();
    await expect(auditor.page.getByRole("button", { name: /^Add rule$|^Text stays inside/ })).toHaveCount(0);
    expect((await api(auditor.page, "PUT", "/platform/workspace-types/team/guardrails", { local_only: false, rules: [] })).status()).toBe(403);
    await expectAccessible(auditor.page, "admin guardrails settings");
  } finally {
    await api(operator.page, "DELETE", platform);
    expect((await api(alex.page, "PUT", local, { rules: [] })).status()).toBe(200);
    for (const session of [alex, blair, operator, auditor]) await session.context.close();
  }
});
