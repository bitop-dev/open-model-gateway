import { expect, test } from "@playwright/test";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";

/*
 * Access review schedules (#481): a Platform Admin creates a schedule in Admin › Access reviews,
 * sees its next start, pauses and resumes it; the Auditor reads it without actions; the admin
 * deletes it. Its first run is the next period start, so it never runs during the spec.
 * Everything this spec creates is its own.
 */
test.afterEach(expectNoCspViolations);

test("a schedule is created, paused, resumed, read by the auditor and deleted", async ({ browser }) => {
  const name = `Browser schedule ${Date.now().toString(36)}`;
  const operator = await signIn(browser, "operator");
  const { page } = operator;
  let id: string | undefined;
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
    await create.getByRole("button", { name: "Create schedule" }).click();
    await expect(create).toBeHidden();

    const schedules = page.getByRole("table", { name: "Review schedules" });
    const row = schedules.getByRole("row").filter({ hasText: name });
    await expect(row).toContainText("Yearly on 29 February (or the month's last day)");
    await expect(row).toContainText("UTC");
    await expect(row).toContainText("Not run yet");
    const listed = await (await api(page, "GET", "/platform/access-review-schedules?limit=200")).json();
    const created = listed.data.find((x: { name: string }) => x.name === name);
    id = created.id as string;
    // The next start is the next 29 February (28 February in a common year), at 00:00 UTC.
    expect(created.next_run_at).toMatch(/-02-2[89]T00:00:00/);
    await expectAccessible(page, "access review schedules");
    await expectNoHorizontalScroll(page);

    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await page.getByRole("menuitem", { name: "Pause…" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Pause" }).click();
    await expect(row.getByText("Paused", { exact: true })).toBeVisible();
    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await page.getByRole("menuitem", { name: "Resume…" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Resume" }).click();
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
    await confirm.getByRole("button", { name: "Delete schedule" }).click();
    await expect(row).toHaveCount(0);
    expect((await api(page, "GET", `/platform/access-review-schedules/${id}`)).status()).toBe(404);
    id = undefined;
  } finally {
    if (id) await api(page, "DELETE", `/platform/access-review-schedules/${id}`);
    await operator.context.close();
  }
});
