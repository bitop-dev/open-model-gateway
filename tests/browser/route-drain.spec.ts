import { expect, test, type Page } from "@playwright/test";
import { api, expectNoCspViolations, signIn } from "./helpers";
import { UPSTREAM_URL } from "./stack.mjs";

// Real signed OIDC, gateway, disposable database and built SPA (#49). A
// Platform Admin drains a route and its connection from Admin and reads the
// drain in words; the ended state replaces only the drain of the gateway's
// own response (a browser test cannot wait for `until`).
test.afterEach(expectNoCspViolations);

async function create(page: Page, path: string, body: unknown) {
  const response = await api(page, "POST", path, body);
  expect(response.status(), `${path}: ${await response.text()}`).toBe(200);
  return (await response.json()).id as string;
}

test("an admin drains a route and its connection and reads the drain in words", async ({ browser }) => {
  const { context, page } = await signIn(browser, "operator");
  try {
    const name = `drain-${Date.now()}`;
    const connection = await create(page, "/platform/providers", { name, provider: "openai_compatible", credential_ref: "none", endpoint: UPSTREAM_URL, enabled: true });
    const model = await create(page, "/platform/models", { public_name: `drain/${name}`, display_name: name, enabled: true, supported_protocols: ["chat_completions"] });
    const deployment = await create(page, "/platform/deployments", { model_id: model, provider_connection_id: connection, upstream_model: name, enabled: true });
    await page.goto(`/admin/deployments/${deployment}`);
    const section = page.locator("#drain");
    await expect(section.getByText("Not drained", { exact: true })).toBeVisible();
    await section.getByRole("button", { name: "Drain route…", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Reason").fill("Replacing the GPU node <b>now</b>");
    await dialog.getByLabel(/Ends after/).fill("2");
    await dialog.getByRole("button", { name: "Drain route", exact: true }).click();
    await expect(dialog).toBeHidden();
    await expect(section.getByText("Drained · takes no new requests", { exact: true })).toBeVisible();
    await expect(section.getByText("Replacing the GPU node <b>now</b>", { exact: true })).toBeVisible();
    await expect(section.getByText("Drained by", { exact: true })).toBeVisible();
    await expect(section.getByText("Until", { exact: true })).toBeVisible();
    await expect(section.getByText("This route takes no new requests now.", { exact: false })).toBeVisible();
    // The API names who drained it and when, and keeps the reason as text.
    const read = await (await api(page, "GET", `/platform/deployments/${deployment}/drain`)).json();
    expect(read.drain).toMatchObject({ reason: "Replacing the GPU node <b>now</b>", state: "active" });
    expect(read.drain.until).toBeTruthy();
    await section.getByRole("button", { name: "End drain", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "End drain", exact: true }).click();
    await expect(section.getByText("Not drained", { exact: true })).toBeVisible();
    // A connection drain: its page counts the routes it takes out, and the
    // route page says the route is drained through it.
    await page.goto(`/admin/providers/${connection}?tab=drain`);
    const panel = page.getByText("Draining it would take out 1 enabled route on this connection.", { exact: false });
    await expect(panel).toBeVisible();
    const drainConnection = page.getByRole("button", { name: "Drain connection…", exact: true });
    await drainConnection.click();
    await page.getByRole("dialog").getByLabel("Reason").fill("Provider incident");
    await page.getByRole("dialog").getByRole("button", { name: "Drain connection", exact: true }).click();
    await expect(page.getByText("Takes out 1 enabled route on this connection.", { exact: false })).toBeVisible();
    await page.goto(`/admin/deployments/${deployment}`);
    await expect(section.getByText(`Drained through its connection ${name}`, { exact: false })).toBeVisible();
    await expect(section.getByText("Provider incident", { exact: true })).toBeVisible();
    // An ended drain reads as ended.
    await page.route(url => url.pathname === `/api/v1/platform/deployments/${deployment}/drain`, async route => {
      const response = await route.fetch();
      const body = await response.json();
      await route.fulfill({ response, json: { ...body, drained: false, drain: { reason: "Old maintenance", until: "2026-01-01T00:00:00Z", state: "ended", drained_at: "2025-12-31T00:00:00Z", drained_by: { id: "u", label: "Operator" } }, connection: { ...body.connection, drain: null } } });
    });
    await page.reload();
    await expect(section.getByText("Drain ended · takes requests again", { exact: true })).toBeVisible();
    await expect(section.getByText("Ended at", { exact: true })).toBeVisible();
    await expect(section.getByText("Drained · takes no new requests", { exact: true })).toHaveCount(0);
  } finally {
    await page.unrouteAll({ behavior: "wait" });
    await context.close();
  }
});
