import { expect, test, type Browser } from "@playwright/test";
import { randomUUID } from "node:crypto";
import pg from "pg";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";

// Resolving an unknown realtime response (#804). A finished realtime session with one live-settled and one unknown
// response is seeded straight into the disposable browser database (the rows a session writes when its last response
// never reported usage), pinned to its own pricing v3 version. A Platform Admin resolves it on the request page through
// the real gateway; an Auditor is never offered the action. Creating keys is setup only.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);

type Seeded = { team: string; root: string; idle: string };
let seeded: Promise<Seeded> | undefined;

async function seed(browser: Browser): Promise<Seeded> {
  const db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  const operator = await signIn(browser, "operator"), alex = await signIn(browser, "alex");
  try {
    const alexId = await (await api(alex.page, "GET", "/me")).json();
    const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name: "Realtime resolution", owner_user_id: alexId.user.id });
    expect(created.status()).toBe(200);
    const team = (await created.json()).id as string;
    const key = await api(alex.page, "POST", `/workspaces/${team}/keys`, { name: "realtime-resolution", expires_in_days: 1, model_ids: null });
    expect(key.status()).toBe(200);
    const keyId = (await key.json()).id as string;
    const { rows: [deployment] } = await db.query("SELECT deployment_id FROM deployment_prices ORDER BY created_at DESC LIMIT 1");
    expect(deployment, "the journey creates a priced route first").toBeDefined();
    // Its own realtime price version, older than the route's current one so later admissions keep that one.
    const line = (meter: string) => ({ meter, microusd_per_batch: "1000000", batch: 1000000, unit_label: "/M tokens", sku_label: "Line" });
    const na = (meter: string) => ({ meter, not_applicable: true });
    const lines = [line("input_tokens"), line("cache_read_tokens"), line("output_tokens"), line("input_audio_tokens"), line("cache_read_audio_tokens"), line("output_audio_tokens"), na("cache_write_tokens"), na("cache_write_5m_tokens"), na("cache_write_1h_tokens"), na("output_images"), na("input_characters"), na("input_audio_seconds_ms"), na("output_audio_seconds_ms"), na("search_units"), { meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request" }];
    const price = randomUUID(), root = randomUUID();
    await db.query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units,created_at) VALUES($1,$2,1000,100,3,$3,'{}',now()-interval '30 days')", [price, deployment.deployment_id, JSON.stringify(lines)]);
    await db.query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,workload_kind,started_at,completed_at) VALUES($1,$2,$3,$4,'realtime-resolution/voice','openai',true,'succeeded',$1,1,'realtime',clock_timestamp()-interval '2 hours',clock_timestamp()-interval '119 minutes')", [root, team, keyId, deployment.deployment_id]);
    await db.query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost,price_id) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),now()-interval '1 hour','unknown',1300,false,$2 FROM inference_executions WHERE id=$1", [root, price]);
    await db.query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd) SELECT $1,execution_id,admitted_at,'unknown',100 FROM governance_reservations WHERE execution_id=$2", [randomUUID(), root]);
    await db.query("INSERT INTO realtime_responses(execution_id,sequence,state,status,window_hold_microusd,actual_microusd,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,completed_at) VALUES($1,1,'settled','completed',100,100,50,0,0,0,50,0,clock_timestamp())", [root]);
    await db.query("INSERT INTO realtime_responses(execution_id,sequence,state,status,window_hold_microusd,input_text_tokens,completed_at) VALUES($1,2,'unknown','completed',1200,40,clock_timestamp())", [root]);
    // A session killed while idle (lease expiry): its one response settled live, its hold that response's actual.
    const idle = randomUUID();
    await db.query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,attempt_number,workload_kind,started_at,completed_at) VALUES($1,$2,$3,$4,'realtime-resolution/voice','openai',true,'cancelled','lease_expired',$1,1,'realtime',clock_timestamp()-interval '3 hours',clock_timestamp()-interval '179 minutes')", [idle, team, keyId, deployment.deployment_id]);
    await db.query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost,price_id) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),now()-interval '2 hours','unknown',100,false,$2 FROM inference_executions WHERE id=$1", [idle, price]);
    await db.query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind) SELECT $1,execution_id,admitted_at,'unknown' FROM governance_reservations WHERE execution_id=$2", [randomUUID(), idle]);
    await db.query("INSERT INTO realtime_responses(execution_id,sequence,state,status,window_hold_microusd,actual_microusd,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,completed_at) VALUES($1,1,'settled','completed',1200,100,50,0,0,0,50,0,clock_timestamp())", [idle]);
    return { team, root, idle };
  } finally {
    await db.end();
    for (const session of [operator, alex]) await session.context.close();
  }
}
const ensure = (browser: Browser) => (seeded ??= seed(browser));

test("an Auditor reads the unknown response but is never offered Resolve", async ({ browser }) => {
  const ids = await ensure(browser), auditor = await signIn(browser, "auditor");
  try {
    await auditor.page.goto(`/admin/logs/${ids.root}`);
    await expect(auditor.page.getByRole("heading", { name: "Realtime responses" })).toBeVisible();
    await expect(auditor.page.getByRole("button", { name: /^Resolve response/ })).toHaveCount(0);
  } finally { await auditor.context.close(); }
});

test("Platform Admin resolves an unknown realtime response on its request page and the session's cost becomes final", async ({ browser }) => {
  const ids = await ensure(browser), operator = await signIn(browser, "operator");
  const page = operator.page, db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  try {
    await page.goto(`/admin/logs/${ids.root}`);
    await expect(page.getByRole("heading", { name: "Realtime responses" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Resolve response 1" })).toHaveCount(0);
    const resolve = page.getByRole("button", { name: "Resolve response 2" });
    await resolve.focus(); await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Resolve response 2" });
    await expect(dialog).toBeVisible();
    await expectAccessible(page, "realtime resolution dialog");
    // The recorded count is prefilled and may not be lowered.
    const text = dialog.getByRole("textbox", { name: /^Text input tokens/ });
    await expect(text).toHaveValue("40");
    for (const [label, value] of [["Cached text input tokens", "0"], ["Audio input tokens", "0"], ["Cached audio input tokens", "0"], ["Text output tokens", "60"], ["Audio output tokens", "0"], ["Authoritative evidence reference", "browser-realtime-ref-1"]] as const) {
      await dialog.getByRole("textbox", { name: new RegExp(`^${label}`) }).fill(value);
    }
    await dialog.getByRole("button", { name: "Resolve response" }).click();
    await expect(dialog).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^Resolve response/ })).toHaveCount(0);
    await expect(page.locator("main")).not.toContainText("No usage reported");
    // The accounting is real: the response and then the session settled, one reconciliation entry, one audit event.
    const { rows: [response] } = await db.query("SELECT state, actual_microusd::text actual, evidence FROM realtime_responses WHERE execution_id=$1 AND sequence=2", [ids.root]);
    expect(response).toMatchObject({ state: "settled", evidence: "browser-realtime-ref-1" });
    const { rows: [session] } = await db.query("SELECT state, actual_microusd::text actual FROM governance_reservations WHERE execution_id=$1", [ids.root]);
    expect(session.state).toBe("settled");
    expect(BigInt(session.actual)).toBe(100n + BigInt(response.actual));
    const { rows: ledger } = await db.query("SELECT kind FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind", [ids.root]);
    expect(ledger.map(l => l.kind)).toEqual(["reconciliation", "unknown"]);
    const { rows: [audit] } = await db.query("SELECT count(*)::int n FROM audit_events WHERE action='realtime_response.reconciled' AND resource_id=$1", [ids.root]);
    expect(audit.n).toBe(1);
    await expectNoHorizontalScroll(page);
    await expectAccessible(page, "realtime responses after resolution");
  } finally { await db.end(); await operator.context.close(); }
});

test("Platform Admin settles a realtime session killed while idle; an Auditor is never offered it", async ({ browser }) => {
  const ids = await ensure(browser), auditor = await signIn(browser, "auditor");
  try {
    await auditor.page.goto(`/admin/logs/${ids.idle}`);
    await expect(auditor.page.getByRole("heading", { name: "Realtime responses" })).toBeVisible();
    await expect(auditor.page.getByRole("button", { name: "Settle session" })).toHaveCount(0);
  } finally { await auditor.context.close(); }
  const operator = await signIn(browser, "operator"), page = operator.page, db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  try {
    await page.goto(`/admin/logs/${ids.idle}`);
    await expect(page.getByRole("button", { name: /^Resolve response/ })).toHaveCount(0);
    await page.getByRole("button", { name: "Settle session" }).click();
    const dialog = page.getByRole("dialog", { name: "Settle realtime session" });
    await expect(dialog).toBeVisible();
    await expectAccessible(page, "realtime session settlement dialog");
    await dialog.getByRole("textbox", { name: /^Authoritative evidence reference/ }).fill("browser-realtime-session-1");
    await dialog.getByRole("button", { name: "Settle session" }).click();
    await expect(dialog).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Settle session" })).toHaveCount(0);
    const { rows: [session] } = await db.query("SELECT state, actual_microusd::text actual FROM governance_reservations WHERE execution_id=$1", [ids.idle]);
    expect(session).toMatchObject({ state: "settled", actual: "100" });
    const { rows: [ledger] } = await db.query("SELECT count(*)::int n FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation' AND evidence='browser-realtime-session-1'", [ids.idle]);
    expect(ledger.n).toBe(1);
    const { rows: [audit] } = await db.query("SELECT count(*)::int n FROM audit_events WHERE action='realtime_session.reconciled' AND resource_id=$1", [ids.idle]);
    expect(audit.n).toBe(1);
    await expectNoHorizontalScroll(page);
  } finally { await db.end(); await operator.context.close(); }
});
