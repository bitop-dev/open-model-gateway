import { expect, test, type Page } from "@playwright/test";
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
/** A layer write names the layer it was built from (#841): read it as `page`'s user sees it now. */
async function built(page: Page, path: string, key: "override" | "local" | "rule_set") {
  const set = (await (await api(page, "GET", path)).json())[key];
  return set ? { rule_set_id: set.id, revision: set.revision } : null;
}
const resetQuery = async (page: Page, path: string) => { const e = await built(page, path, "override"); return e ? `?rule_set_id=${e.rule_set_id}&revision=${e.revision}` : ""; };
test.use({ trace: "off", screenshot: "off", video: "off" });

test("a workspace rule blocks a request and the admin pages show the layers", async ({ browser }) => {
  const ws = readFixture().product!, unique = Date.now().toString(36);
  const rule = `Ticket codes ${unique}`, local = `/workspaces/${ws}/guardrails`, platform = `/platform/workspaces/${ws}/guardrails`;
  const alex = await signIn(browser, "alex"), blair = await signIn(browser, "blair"), operator = await signIn(browser, "operator"), auditor = await signIn(browser, "auditor");
  try {
    // A platform override with a monitoring secrets rule, for the workspace to tighten.
    expect((await api(operator.page, "PUT", platform, { local_only: false, expected: await built(operator.page, platform, "override"), rules: [{ name: "Secrets", protection: "secrets", detector: { kind: "pattern", categories: ["secret"] }, input: { action: "monitor" }, workloads: ["generation", "embeddings"] }] })).status()).toBe(200);
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
    // Tightening the platform rule and renaming the custom one send the layer's
    // entries back as stored: the server accepts them and keeps their settings.
    await alex.page.getByRole("button", { name: "Tighten Secrets" }).click();
    const tighten = alex.page.getByRole("dialog", { name: "Tighten Secrets" });
    await expect(tighten.getByRole("switch")).toHaveCount(0);
    await tighten.getByRole("button", { name: "Tighten" }).click();
    await expect(tighten).toBeHidden();
    await expect(effective.getByText("Tightened by this workspace")).toBeVisible();
    await alex.page.getByRole("button", { name: `Edit ${rule}` }).click();
    const edit = alex.page.getByRole("dialog", { name: `Edit ${rule}` });
    // Block's default (refuse): shown as the default.
    await expect(edit.getByRole("combobox", { name: "If the rule can't run" })).toHaveValue("");
    await edit.getByRole("textbox", { name: "Name" }).fill(`${rule} (edited)`);
    await edit.getByRole("button", { name: "Save rule" }).click();
    await expect(edit).toBeHidden();
    const stored = (await (await api(alex.page, "GET", local)).json()).local.rules;
    expect(stored).toEqual([
      expect.objectContaining({ name: `${rule} (edited)`, on_error: "fail_closed", input: expect.objectContaining({ action: "block" }), detector: expect.objectContaining({ patterns: ["TICKET-[0-9]{4}"] }) }),
      { id: expect.any(String), tightens: expect.any(String), input: { action: "block" } },
    ]);
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

    // Picking the Guardrails filter keeps it in the URL, and switching tabs and back keeps it too.
    await blair.page.goto(`/workspaces/${ws}/logs`);
    // The list and its tiles have loaded (a re-render while the filter list is open would close it).
    await expect(blair.page.getByText("Refused by guardrails").first()).toBeVisible();
    await blair.page.waitForLoadState("networkidle");
    await blair.page.getByRole("button", { name: /^More filters/ }).click();
    const facet = blair.page.getByRole("combobox", { name: "Guardrails" });
    await facet.click();
    await facet.pressSequentially("Refused");
    await expect(blair.page.getByRole("option", { name: "Refused", exact: true })).toBeVisible();
    await blair.page.keyboard.press("ArrowDown");
    await blair.page.keyboard.press("Enter");
    await expect(blair.page).toHaveURL(/[?&]guardrail=refused/);
    await blair.page.keyboard.press("Escape");
    await expect(blair.page.getByText("Refused by guardrails").first()).toBeVisible();
    await blair.page.getByRole("tab", { name: "Generations" }).click();
    await expect(blair.page).toHaveURL(/tab=generations/);
    await blair.page.getByRole("tab", { name: "Requests" }).click();
    await expect(blair.page).toHaveURL(/[?&]guardrail=refused/);
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

    // A Platform Admin resets the Team's override (the workspace's tightening of
    // it stops applying), then overrides the default again and resets it.
    await operator.page.goto(`/admin/teams/${ws}?tab=guardrails`);
    await expect(operator.page.getByRole("heading", { name: "Platform override" })).toBeVisible();
    await expect(operator.page.getByRole("table", { name: "This workspace's rules" }).getByText(rule)).toBeVisible();
    await expect(operator.page.getByRole("table", { name: "Guardrail decisions this month" }).getByText("Blocked")).toBeVisible();
    const reset = async (warning?: string) => {
      await operator.page.getByRole("button", { name: "Reset to default" }).click();
      const dialog = operator.page.getByRole("dialog", { name: "Reset to the default?" });
      if (warning) await expect(dialog.getByText(warning, { exact: false })).toBeVisible();
      await dialog.getByRole("button", { name: "Reset" }).click();
      await expect(operator.page.getByRole("heading", { name: "Team default" })).toBeVisible();
      expect((await (await api(operator.page, "GET", platform)).json()).override).toBeNull();
    };
    await reset("This workspace's tightening of the override's rules stops applying");
    await alex.page.setViewportSize({ width: 1440, height: 900 });
    await alex.page.reload();
    await expect(effective.getByText("Off: its platform rule was removed")).toBeVisible();
    await operator.page.getByRole("button", { name: "Override the default" }).click();
    await operator.page.getByRole("dialog", { name: "Override the default for this workspace?" }).getByRole("button", { name: "Create override" }).click();
    await expect(operator.page.getByRole("heading", { name: "Platform override" })).toBeVisible();
    expect((await (await api(operator.page, "GET", platform)).json()).override).not.toBeNull();
    await alex.page.reload();
    await expect(alex.page.getByText("Set by a Platform Admin for this workspace, with this workspace's own rules.")).toBeVisible();
    await reset();
    // The workspace's tightening names a rule that is gone: it is shown as
    // such, and the layer still saves (without it).
    await alex.page.reload();
    await expect(alex.page.getByText(/Its platform rule was removed: it no longer applies/)).toBeVisible();
    await alex.page.getByRole("button", { name: `Remove ${rule} (edited)` }).click();
    await alex.page.getByRole("dialog", { name: `Remove ${rule} (edited)?` }).getByRole("button", { name: "Remove" }).click();
    await expect(alex.page.getByText("No rules of this workspace's own.")).toBeVisible();
    expect((await (await api(alex.page, "GET", local)).json()).local.rules).toEqual([]);
    await expectAccessible(operator.page, "admin team guardrails");

    // A tightening the platform rule later outgrows: the workspace tightens an
    // Off rule to Monitor, a Platform Admin then raises the rule itself to
    // Block. The tightening is marked as weaker, and the next local save
    // leaves it out instead of being refused.
    expect((await api(operator.page, "PUT", platform, { local_only: false, expected: await built(operator.page, platform, "override"), rules: [{ name: "Secrets", protection: "secrets", detector: { kind: "pattern", categories: ["secret"] }, input: { action: "off" } }] })).status()).toBe(200);
    const secretsRule = (await (await api(operator.page, "GET", platform)).json()).override.rules[0];
    await alex.page.reload();
    await alex.page.getByRole("button", { name: "Tighten Secrets" }).click();
    const tightenOff = alex.page.getByRole("dialog", { name: "Tighten Secrets" });
    await tightenOff.getByRole("combobox", { name: "On input" }).selectOption("monitor");
    await tightenOff.getByRole("button", { name: "Tighten" }).click();
    await expect(effective.getByText("Tightened by this workspace")).toBeVisible();
    expect((await api(operator.page, "PUT", platform, { local_only: false, expected: await built(operator.page, platform, "override"), rules: [{ id: secretsRule.id, name: "Secrets", protection: "secrets", detector: { kind: "pattern", categories: ["secret"] }, input: { action: "block" } }] })).status()).toBe(200);
    await alex.page.reload();
    await expect(alex.page.getByText(/“Secrets” is now stricter than this: it no longer applies/)).toBeVisible();
    await alex.page.getByRole("button", { name: "Add rule" }).click();
    const late = alex.page.getByRole("dialog", { name: "Add a rule to this workspace's rules" });
    await late.getByRole("textbox", { name: "Name" }).fill(`Late rule ${unique}`);
    await late.getByRole("button", { name: "Add rule" }).click();
    await expect(late).toBeHidden();
    expect((await (await api(alex.page, "GET", local)).json()).local.rules).toEqual([expect.objectContaining({ name: `Late rule ${unique}` })]);

    // #841: a personal-data rule looks only for the kinds ticked (none to
    // begin with): Block on payment cards refuses a card and lets an email through.
    await alex.page.getByRole("button", { name: "Add rule" }).click();
    const pd = alex.page.getByRole("dialog", { name: "Add a rule to this workspace's rules" });
    await pd.getByRole("textbox", { name: "Name" }).fill(`No cards ${unique}`);
    await pd.getByRole("combobox", { name: "Looks for" }).selectOption("personal");
    const boxes = pd.getByRole("checkbox", { name: /^(Email|Phone|Payment card|IBAN|US SSN|IP address):/ });
    await expect(boxes).toHaveCount(6);
    for (const box of await boxes.all()) await expect(box).not.toBeChecked();
    await expect(pd.getByText(/Public IP addresses only/)).toBeVisible();
    await pd.getByRole("checkbox", { name: /^Payment card:/ }).check();
    await pd.getByRole("combobox", { name: "On input" }).selectOption("block");
    await pd.getByRole("button", { name: "Add rule" }).click();
    await expect(pd).toBeHidden();
    await expect(effective.getByText(/Looks for personal data: payment card numbers/)).toBeVisible();
    const card = await send("charge 4111 1111 1111 1111 please");
    expect(card.status).toBe(400);
    expect(JSON.stringify(await card.json())).not.toContain("4111");
    expect((await send("mail alice.smith@acme-corp.io")).status).toBe(200);
    const refusedCard = (await (await api(blair.page, "GET", `/workspaces/${ws}/requests?status=blocked&limit=5`)).json()).data[0];
    await blair.page.goto(`/workspaces/${ws}/requests/${refusedCard.root_request_id}`);
    await expect(blair.page.getByRole("table", { name: "Guardrail decisions" }).getByText("Personal data · Payment card")).toBeVisible();

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
    await api(operator.page, "DELETE", `${platform}${await resetQuery(operator.page, platform)}`);
    expect((await api(alex.page, "PUT", local, { expected: await built(alex.page, local, "local"), rules: [] })).status()).toBe(200);
    for (const session of [alex, blair, operator, auditor]) await session.context.close();
  }
});
