import { expect, test, type Page } from "@playwright/test";
import { api, expectNoCspViolations, signIn } from "./helpers";
import { UPSTREAM_URL } from "./stack.mjs";

// Real signed OIDC, gateway, disposable database and built SPA. The route's
// health is the gateway's own response for the seeded route, with only its
// `health` replaced per state (each state needs upstream failures or the
// passing of a cooldown, which a browser test cannot wait for). The states
// are read in words, never by colour alone (#49).
test.afterEach(expectNoCspViolations);

async function create(page: Page, path: string, body: unknown) {
  const response = await api(page, "POST", path, body);
  expect(response.status(), `${path}: ${await response.text()}`).toBe(200);
  return (await response.json()).id as string;
}

const later = (seconds: number) => new Date(Date.now() + seconds * 1000).toISOString();

const states: { name: string; health: Record<string, unknown>; shows: string[] }[] = [
  { name: "never observed", health: {}, shows: ["Unknown · no traffic observed yet"] },
  { name: "stale", health: { last_observed_at: later(-1200), observation: "stale" }, shows: ["Unknown lately · no traffic observed for over 10 minutes"] },
  { name: "closed", health: { last_observed_at: later(-5), observation: "current", consecutive_failures: 1 }, shows: ["Closed · 1 consecutive observed failure"] },
  { name: "cooling down", health: { last_observed_at: later(-5), observation: "current", consecutive_failures: 3, circuit: "open", open_until: later(120), cooling_until: later(120), reopen_count: 2 }, shows: ["Cooling down · left out of routing", "Cooling until", "Backoff", "Opened 2 times since its last complete success"] },
  { name: "half-open", health: { last_observed_at: later(-5), observation: "current", consecutive_failures: 3, circuit: "half_open", open_until: later(-1), reopen_count: 2 }, shows: ["Half-open · waiting for a request to test it", "Opened 2 times since its last complete success"] },
  { name: "probe in progress", health: { last_observed_at: later(-5), observation: "current", consecutive_failures: 3, circuit: "half_open", open_until: later(-1), probe_in_progress: true, probe_until: later(120), reopen_count: 2 }, shows: ["Half-open · one request is testing it", "Test ends by"] },
];

test("a route's health reads its circuit, probe and observation in words", async ({ browser }) => {
  const { context, page } = await signIn(browser, "operator");
  try {
    const name = `health-${Date.now()}`;
    const connection = await create(page, "/platform/providers", { name, provider: "openai_compatible", credential_ref: "none", endpoint: UPSTREAM_URL, enabled: true });
    const model = await create(page, "/platform/models", { public_name: `health/${name}`, display_name: name, enabled: true, supported_protocols: ["chat_completions"] });
    const deployment = await create(page, "/platform/deployments", { model_id: model, provider_connection_id: connection, upstream_model: name, enabled: true });
    let patch: Record<string, unknown> = {};
    await page.route(url => url.pathname === `/api/v1/platform/deployments/${deployment}/routing`, async route => {
      if (route.request().method() !== "GET") { await route.continue(); return; }
      const response = await route.fetch();
      const body = await response.json();
      // The real view of a route that never failed, as the gateway computed it.
      expect(body.health).toMatchObject({ circuit: "closed", observation: "none", probe_in_progress: false, reopen_count: 0 });
      expect(body.health).not.toHaveProperty("probe_holder");
      await route.fulfill({ response, json: { ...body, health: { ...body.health, ...patch } } });
    });
    for (const state of states) {
      patch = state.health;
      await page.goto(`/admin/deployments/${deployment}`);
      const section = page.locator("#routing");
      for (const text of state.shows) await expect(section.getByText(text, { exact: false }).first(), `${state.name}: ${text}`).toBeVisible();
      await expect(section.getByText("Health observations do not establish current upstream readiness.")).toBeVisible();
      await expect(section.getByText("As of", { exact: true })).toBeVisible();
      if (state.name === "never observed") await expect(section.getByText("Never", { exact: true })).toBeVisible();
      // The summary tile names the same state.
      await expect(page.getByRole("group", { name: "Route summary" }).getByText(state.shows[0], { exact: false })).toBeVisible();
    }
  } finally {
    await page.unrouteAll({ behavior: "wait" });
    await context.close();
  }
});
