import { expect, test } from "@playwright/test";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";

/*
 * Access review schedules (#481): a Platform Admin creates a schedule in Admin › Access reviews,
 * sees its next start, pauses and resumes it; the Auditor reads it without actions; the admin
 * deletes it. Its first run is the next period start, so it never runs during the spec.
 * Everything this spec creates is its own.
 */
test.afterEach(expectNoCspViolations);

/*
 * While a dialog is open the page behind it is inert, so a role query finds none of its rows: a
 * row assertion made then passes without the change having happened. Each dialog step therefore
 * waits for its own request's response (bounded by the test's timeout, not the shorter expect
 * timeout: a management write can wait behind the installation lock), then for the dialog to
 * close, and only then reads the table.
 */
const answered = (page: import("@playwright/test").Page, method: string, id: string) =>
  page.waitForResponse(r => r.request().method() === method && new URL(r.url()).pathname === `/api/v1/platform/access-review-schedules/${id}`, { timeout: 0 });

test("a schedule is created, paused, resumed, read by the auditor and deleted", async ({ browser }) => {
  const name = `Browser schedule ${Date.now().toString(36)}`;
  const operator = await signIn(browser, "operator");
  const { page } = operator;
  let id: string | undefined;
  let failed = false;
  try {
    await page.goto("/admin/access-reviews");
    await page.getByRole("button", { name: "New schedule" }).click();
    const create = page.getByRole("dialog", { name: "New review schedule" });
    await create.getByRole("textbox", { name: "Name" }).fill(name);
    await create.getByRole("checkbox", { name: "Admin and Auditor roles" }).check();
    await create.getByRole("checkbox", { name: "Team and Project memberships" }).uncheck();
    await create.getByRole("combobox", { name: /^Repeats/ }).selectOption("yearly");
    await create.getByRole("combobox", { name: /^Start month/ }).selectOption("2");
    await create.getByRole("spinbutton", { name: /^Start day/ }).fill("29");
    // The id comes from the create response, before any assertion that could fail and leave it behind.
    const posted = page.waitForResponse(r => r.request().method() === "POST" && new URL(r.url()).pathname === "/api/v1/platform/access-review-schedules");
    await create.getByRole("button", { name: "Create schedule" }).click();
    const response = await posted;
    expect(response.status()).toBe(200);
    const created = await response.json();
    id = created.id as string;
    await expect(create).toBeHidden();

    const schedules = page.getByRole("table", { name: "Review schedules" });
    const row = schedules.getByRole("row").filter({ hasText: name });
    await expect(row).toContainText("Yearly on 29 February (or the month's last day)");
    await expect(row).toContainText("UTC");
    await expect(row).toContainText("Not run yet");
    // The next start is the next 29 February (28 February in a common year), at 00:00 UTC.
    expect(created.next_run_at).toMatch(/-02-2[89]T00:00:00/);
    await expectAccessible(page, "access review schedules");
    await expectNoHorizontalScroll(page);

    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await page.getByRole("menuitem", { name: "Pause…" }).click();
    const paused = answered(page, "PATCH", id);
    await page.getByRole("dialog").getByRole("button", { name: "Pause" }).click();
    expect((await paused).status()).toBe(200);
    await expect(page.getByRole("dialog")).toBeHidden();
    await expect(row.getByText("Paused", { exact: true })).toBeVisible();
    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await page.getByRole("menuitem", { name: "Resume…" }).click();
    const resumed = answered(page, "PATCH", id);
    await page.getByRole("dialog").getByRole("button", { name: "Resume" }).click();
    expect((await resumed).status()).toBe(200);
    await expect(page.getByRole("dialog")).toBeHidden();
    await expect(row).toHaveCount(1);
    await expect(row.getByText("Paused", { exact: true })).toHaveCount(0);

    const auditor = await signIn(browser, "auditor");
    try {
      await auditor.page.goto("/admin/access-reviews");
      const seen = auditor.page.getByRole("table", { name: "Review schedules" }).getByRole("row").filter({ hasText: name });
      await expect(seen).toContainText("Yearly on 29 February");
      await expect(auditor.page.getByRole("button", { name: /^New schedule$|^Actions for / })).toHaveCount(0);
    } finally { await auditor.context.close(); }

    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await page.getByRole("menuitem", { name: "Delete…" }).click();
    const confirm = page.getByRole("dialog");
    await expect(confirm).toContainText("Reviews it already started stay as they are.");
    const deleted = answered(page, "DELETE", id);
    await confirm.getByRole("button", { name: "Delete schedule" }).click();
    expect((await deleted).status()).toBe(200);
    await expect(confirm).toBeHidden();
    await expect(row).toHaveCount(0);
    expect((await api(page, "GET", `/platform/access-review-schedules/${id}`)).status()).toBe(404);
    id = undefined;
  } catch (error) {
    failed = true;
    throw error;
  } finally {
    // Only removes what a failed step left behind. A schedule the dialog already deleted answers
    // 404, the outcome wanted here. Any other cleanup failure is the test's error only when no
    // step failed; after a failed step it is reported beside that step's error, never instead.
    try {
      if (id) expect([200, 404]).toContain((await api(page, "DELETE", `/platform/access-review-schedules/${id}`)).status());
    } catch (cleanup) {
      if (!failed) throw cleanup;
      test.info().annotations.push({ type: "cleanup failed", description: String(cleanup) });
    } finally { await operator.context.close(); }
  }
});
