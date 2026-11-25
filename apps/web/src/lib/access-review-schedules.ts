/*
 * Access review schedules (#481): a campaign template that starts an
 * ordinary access review each period (docs/management-api.md "Schedules
 * (#481)"). Periods start at 00:00 UTC on the start day, or on the month's
 * last day when the start day is past its end. Platform Admins manage
 * schedules; Platform Auditors read them.
 */
import { platformPath } from "./api";
import type { ReviewSubject } from "./access-reviews";

export type Cadence = "monthly" | "quarterly" | "yearly";
export type SkipReason = "previous_open" | "open_limit" | "empty" | "too_large" | "changed";
export type ScheduleRun = { id: string; period_at: string; ran_at: string; outcome: "started" | "skipped"; reason: SkipReason | null; campaign_id: string | null };
export type ScheduleCampaign = { id: string; name: string; created_at: string; due_at: string; closed_at: string | null };
export type ReviewSchedule = {
  id: string; name: string; subjects: ReviewSubject[]; workspace_ids: string[]; open_days: number;
  cadence: Cadence; start_day: number; start_month: number | null;
  created_by: string; created_at: string; updated_at: string;
  /** Null while active; the next period start is then `next_run_at` (null while paused). */
  paused_at: string | null; next_run_at: string | null;
  last_run: ScheduleRun | null; last_campaign: ScheduleCampaign | null;
};

export const schedulesPath = `${platformPath}/access-review-schedules`;
export const schedulePath = (id: string) => `${schedulesPath}/${encodeURIComponent(id)}`;

export const cadenceLabels: Record<Cadence, string> = { monthly: "Monthly", quarterly: "Quarterly", yearly: "Yearly" };
export const monthNames = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"] as const;
const quarterMonths = (start: number) => [0, 3, 6, 9].map(i => monthNames[(start - 1 + i) % 12]!.slice(0, 3)).join(", ");

/** "Monthly on day 1", "Quarterly on day 31 of Feb, May, Aug, Nov", "Yearly on 15 November", in UTC. */
export function cadenceText(s: Pick<ReviewSchedule, "cadence" | "start_day" | "start_month">): string {
  const day = s.start_day, month = s.start_month ?? 1;
  const text = s.cadence === "monthly" ? `Monthly on day ${day}`
    : s.cadence === "quarterly" ? `Quarterly on day ${day} of ${quarterMonths(month)}`
    : `Yearly on ${day} ${monthNames[month - 1]}`;
  return day > 28 ? `${text} (or the month's last day)` : text;
}

export const skipReasonLabels: Record<SkipReason, string> = {
  previous_open: "the previous review was still open",
  open_limit: "20 reviews were open",
  empty: "nothing matched",
  too_large: "over 5,000 items",
  changed: "access changed during the snapshot",
};
export function lastRunText(run: ScheduleRun | null): string {
  if (!run) return "Not run yet";
  return run.outcome === "started" ? "Started a review" : `Skipped: ${skipReasonLabels[run.reason ?? "empty"]}`;
}

/** A period start or run time, as a UTC date (periods are defined in UTC). */
export const utcDate = (iso: string) => `${new Date(iso).toLocaleDateString(undefined, { timeZone: "UTC", year: "numeric", month: "short", day: "numeric" })} UTC`;

/** The schedule's state for its badge. */
export const scheduleState = (s: ReviewSchedule) => s.paused_at ? "paused" as const : "active" as const;

/** The calendar part of a create or change body: monthly takes no start month. */
export function calendarBody(values: { cadence?: string; start_day?: string; start_month?: string }) {
  const cadence = (values.cadence ?? "monthly") as Cadence;
  return { cadence, start_day: Number(values.start_day), start_month: cadence === "monthly" ? null : Number(values.start_month) };
}
