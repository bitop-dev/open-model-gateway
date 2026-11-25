// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { DashboardNavigationProvider } from "./navigation-link";
import { AccessReviewDetail, PlatformAccessReviewsPage } from "../pages/access-reviews";
import { reviewItemsPath, reviewPath, reviewsPath, type ReviewCampaignDetail } from "../lib/access-reviews";
import { cadenceText, calendarBody, lastRunText, schedulePath, schedulesPath, utcDate, type ReviewSchedule } from "../lib/access-review-schedules";
import { admin, auditor, testClient } from "../lib/test-fixtures";

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
/** Every write: method, path and body. */
const writes = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls
  .filter(([, init]) => ["POST", "PATCH", "DELETE"].includes((init as RequestInit | undefined)?.method ?? ""))
  .map(([path, init]) => [(init as RequestInit).method, path, JSON.parse(String((init as RequestInit).body ?? "null"))]);
function mount(node: React.ReactElement) {
  render(<QueryClientProvider client={testClient()}><DashboardNavigationProvider search={{ page: "access-reviews" }} navigate={vi.fn()}><ActionProvider>{node}</ActionProvider></DashboardNavigationProvider></QueryClientProvider>);
  return userEvent.setup();
}

const schedule: ReviewSchedule = {
  id: "s-1", name: "Monthly team review", subjects: ["workspace_memberships"], workspace_ids: ["team"], open_days: 14,
  cadence: "monthly", start_day: 1, start_month: null, created_by: "admin", created_at: "2026-11-01T00:00:00Z", updated_at: "2026-11-01T00:00:00Z",
  paused_at: null, next_run_at: "2026-12-01T00:00:00Z",
  last_run: { id: "r-1", period_at: "2026-11-01T00:00:00Z", ran_at: "2026-11-01T00:00:30Z", outcome: "started", reason: null, campaign_id: "c-1" },
  last_campaign: { id: "c-1", name: "Monthly team review (2026-11-01)", created_at: "2026-11-01T00:00:30Z", due_at: "2026-11-15T00:00:30Z", closed_at: null },
};
const skipped: ReviewSchedule = {
  ...schedule, id: "s-2", name: "Quarterly roles", subjects: ["platform_roles"], workspace_ids: [], cadence: "quarterly", start_day: 31, start_month: 2,
  last_run: { id: "r-2", period_at: "2026-11-30T00:00:00Z", ran_at: "2026-11-30T00:00:30Z", outcome: "skipped", reason: "previous_open", campaign_id: null },
  last_campaign: { ...schedule.last_campaign!, id: "c-2", name: "Quarterly roles (2026-08-31)" },
};
const paused: ReviewSchedule = { ...schedule, id: "s-3", name: "Paused yearly", cadence: "yearly", start_day: 15, start_month: 11, paused_at: "2026-11-20T00:00:00Z", next_run_at: null, last_run: null, last_campaign: null };

describe("Review schedules: wording (#481)", () => {
  it("describes the calendar in UTC terms, including a start day past a month's end", () => {
    expect(cadenceText(schedule)).toBe("Monthly on day 1");
    expect(cadenceText(skipped)).toBe("Quarterly on day 31 of Feb, May, Aug, Nov (or the month's last day)");
    expect(cadenceText(paused)).toBe("Yearly on 15 November");
    expect(utcDate("2026-12-01T00:00:00Z")).toMatch(/2026.*UTC$/);
  });
  it("says what the last run did, with the reason of a skip", () => {
    expect(lastRunText(null)).toBe("Not run yet");
    expect(lastRunText(schedule.last_run)).toBe("Started a review");
    expect(lastRunText(skipped.last_run)).toBe("Skipped: the previous review was still open");
  });
  it("sends no start month for a monthly calendar", () => {
    expect(calendarBody({ cadence: "monthly", start_day: "5", start_month: "3" })).toEqual({ cadence: "monthly", start_day: 5, start_month: null });
    expect(calendarBody({ cadence: "yearly", start_day: "29", start_month: "2" })).toEqual({ cadence: "yearly", start_day: 29, start_month: 2 });
  });
});

describe("Admin › Access reviews › Schedules (#481)", () => {
  it("lists each schedule with its next start, last run (and a skip's reason) and last review", async () => {
    serve({ [reviewsPath]: () => ({ data: [] }), [schedulesPath]: () => ({ data: [schedule, skipped, paused] }) });
    mount(<PlatformAccessReviewsPage session={admin} />);
    const table = await screen.findByRole("table", { name: "Review schedules" });
    const row = (name: string) => within(table).getByRole("rowheader", { name: new RegExp(name) }).closest("tr")!;
    await within(table).findByText("Monthly team review");
    expect(row("Monthly team review").textContent).toContain("Monthly on day 1");
    expect(row("Monthly team review").textContent).toContain(utcDate("2026-12-01T00:00:00Z"));
    expect(within(row("Monthly team review")).getByRole("link", { name: "Monthly team review (2026-11-01)" })).toBeTruthy();
    expect(row("Quarterly roles").textContent).toContain("Skipped: the previous review was still open");
    expect(row("Paused yearly").textContent).toContain("Paused");
    expect(row("Paused yearly").textContent).toContain("Not run yet");
    expect(row("Paused yearly").textContent).toContain("None yet");
  });
  it("creates a quarterly schedule with what the admin chose", async () => {
    const fetch = serve({ [reviewsPath]: () => ({ data: [] }), [schedulesPath]: init => init?.method === "POST" ? { ...schedule, cadence: "quarterly" } : { data: [] } });
    const user = mount(<PlatformAccessReviewsPage session={admin} />);
    await user.click(await screen.findByRole("button", { name: /New schedule/ }));
    const dialog = await screen.findByRole("dialog", { name: "New review schedule" });
    await user.type(within(dialog).getByRole("textbox", { name: "Name" }), "Roles each quarter");
    await user.click(within(dialog).getByRole("checkbox", { name: "Admin and Auditor roles" }));
    await user.click(within(dialog).getByRole("checkbox", { name: "Team and Project memberships" }));
    expect(within(dialog).queryByRole("combobox", { name: /^Start month/ })).toBeNull();
    await user.selectOptions(within(dialog).getByRole("combobox", { name: /^Repeats/ }), "quarterly");
    await user.selectOptions(within(dialog).getByRole("combobox", { name: /^Start month/ }), "2");
    const day = within(dialog).getByRole("spinbutton", { name: /^Start day/ });
    await user.clear(day);
    await user.type(day, "31");
    await user.click(within(dialog).getByRole("button", { name: "Create schedule" }));
    await waitFor(() => expect(writes(fetch)).toEqual([["POST", schedulesPath, { name: "Roles each quarter", subjects: ["platform_roles"], workspace_ids: [], open_days: 14, cadence: "quarterly", start_day: 31, start_month: 2 }]]));
  });
  it("changes, pauses, resumes and deletes a schedule", async () => {
    const fetch = serve({ [reviewsPath]: () => ({ data: [] }), [schedulesPath]: () => ({ data: [schedule, paused] }), [schedulePath("s-1")]: () => schedule, [schedulePath("s-3")]: () => paused });
    const user = mount(<PlatformAccessReviewsPage session={admin} />);
    const act = async (name: string, item: string, submit: string) => {
      await user.click(await screen.findByRole("button", { name: `Actions for ${name}` }));
      await user.click(await screen.findByRole("menuitem", { name: item }));
      const dialog = await screen.findByRole("dialog");
      await user.click(within(dialog).getByRole("button", { name: submit }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    };
    await act("Monthly team review", "Pause…", "Pause");
    // A paused schedule offers Resume, not Pause.
    await user.click(await screen.findByRole("button", { name: "Actions for Paused yearly" }));
    const resume = await screen.findByRole("menuitem", { name: "Resume…" });
    expect(screen.queryByRole("menuitem", { name: "Pause…" })).toBeNull();
    await user.click(resume);
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Resume" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await user.click(await screen.findByRole("button", { name: "Actions for Monthly team review" }));
    await user.click(await screen.findByRole("menuitem", { name: "Change…" }));
    const dialog = await screen.findByRole("dialog", { name: "Change Monthly team review" });
    const day = within(dialog).getByRole("spinbutton", { name: /^Start day/ });
    await user.clear(day);
    await user.type(day, "5");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await act("Monthly team review", "Delete…", "Delete schedule");
    expect(writes(fetch)).toEqual([
      ["PATCH", schedulePath("s-1"), { paused: true }],
      ["PATCH", schedulePath("s-3"), { paused: false }],
      ["PATCH", schedulePath("s-1"), { name: "Monthly team review", open_days: 14, cadence: "monthly", start_day: 5, start_month: null }],
      ["DELETE", schedulePath("s-1"), null],
    ]);
  });
  it("is read-only for an Auditor", async () => {
    serve({ [reviewsPath]: () => ({ data: [] }), [schedulesPath]: () => ({ data: [schedule] }) });
    mount(<PlatformAccessReviewsPage session={auditor} />);
    await screen.findByText("Monthly team review");
    expect(screen.queryByRole("button", { name: /New schedule/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Actions for Monthly team review" })).toBeNull();
  });
  it("says on a review's page that a schedule started it", async () => {
    const detail: ReviewCampaignDetail = { id: "c-1", name: "Monthly team review (2026-11-01)", subjects: ["workspace_memberships"], workspace_ids: ["team"], created_by: "admin", created_at: "2026-11-01T00:00:30Z", due_at: "2099-11-15T00:00:30Z", closed_at: null, item_count: 0, schedule_id: "s-1",
      counts: { by_kind: { workspace_membership: 0, platform_role: 0, workspace_key: 0 }, by_source: { manual: 0, group: 0, bootstrap: 0 }, by_decision: { pending: 0, keep: 0, flag_idp: 0, remove: 0 }, inactive: 0, without_reviewer: 0 } };
    serve({ [reviewPath("c-1")]: () => detail, [reviewItemsPath("c-1")]: () => ({ data: [] }) });
    mount(<AccessReviewDetail session={admin} id="c-1" />);
    expect(await screen.findByText(/started by a schedule/)).toBeTruthy();
  });
});
