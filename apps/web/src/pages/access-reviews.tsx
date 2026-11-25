/*
 * Access reviews (#90). Three surfaces, browser-session only
 * (docs/management-api.md#access-reviews-90):
 *   - Review access (/access-reviews): the items the signed-in person may
 *     decide now: keep, remove (applied at once, after a confirmation that
 *     says what it does) or flag group-provided access for the identity
 *     provider. A key is kept or revoked.
 *   - Admin › Access reviews: campaigns, and starting one (Platform Admins);
 *     schedules that start one each period (#481): Platform Admins create,
 *     change, pause, resume and delete them; Auditors read them.
 *   - A campaign: its counts and items with each latest decision and
 *     reviewer; Platform Admins assign a reviewer and close it. Keys show
 *     no name or holder here and are never assigned.
 * The server decides who may review what and refuses anything else; the
 * dashboard offers only the steps the viewer can take.
 */
import { CalendarClock, ClipboardCheck, Plus } from "lucide-react";
import { api, platformPath, type Session } from "../lib/api";
import { accessText, decisionText, isKey, keyIdentity, keyHolderText, keyText, keyUseText, subjectsText, itemLabel, removalTarget, campaignState, campaignStateLabels, campaignStateTone, decisionPath, decisionTone, inboxPath, personText, removalConsequence, reviewClosePath, reviewExportPath, reviewerPath, reviewItemsPath, reviewPath, reviewsPath, sourceLabels, subjectLabels, type ReviewSubject, type InboxItem, type ReviewCampaign, type ReviewCampaignDetail, type ReviewItem } from "../lib/access-reviews";
import { cadenceLabels, cadenceText, calendarBody, lastRunText, monthNames, schedulePath, schedulesPath, scheduleState, utcDate, type ReviewSchedule } from "../lib/access-review-schedules";
import { nameField, type Field, type Values } from "../lib/forms";
import type { DirectoryUser, DirectoryWorkspace } from "../lib/people";
import { Button, ErrorNotice, Heading, Inline, Stack, useAction, useApi } from "../components/ui";
import { DirectoryTable } from "../components/people";
import { ResourceLink } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { StatusBadge } from "../components/ui/badge/badge";
import { TextLink } from "../components/ui/text-link/text-link";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import s from "./shared.module.css";

function DecisionCell({ item }: { item: ReviewItem }) {
  if (item.decision) return <Inline gap={2}><StatusBadge tone={decisionTone[item.decision]}>{decisionText(item, item.decision)}</StatusBadge>{!item.active && item.decision !== "remove" && <span className={s.muted}>No longer in effect</span>}</Inline>;
  return item.active ? <span className={s.muted}>Not decided</span> : <span className={s.muted}>No longer in effect</span>;
}
const personColumn = <T extends ReviewItem>(): DataTableColumn<T> => ({ id: "person", header: "Person or key", accessor: personText, rowHeader: true, hideable: false, cell: i => <CellText primary={personText(i)} secondary={isKey(i) ? keyHolderText(i) : i.display_name ? i.email ?? undefined : undefined} /> });
const accessColumn = <T extends ReviewItem>(): DataTableColumn<T> => ({ id: "access", header: "Access", accessor: accessText, cell: i => <CellText primary={accessText(i)} secondary={isKey(i) ? keyUseText(i) : i.source ? sourceLabels[i.source] : undefined} /> });

/**
 * Removing an owner's membership needs an owner of that workspace or a
 * Platform Admin (as on the members routes); the server refuses others.
 */
function mayRemove(session: Session, i: ReviewItem): boolean {
  if (isKey(i)) return true;
  if (i.source === "group") return false;
  if (i.subject_kind !== "workspace_membership" || i.role !== "owner") return true;
  return session.capabilities.platform_write || session.workspaces.some(w => w.id === i.workspace_id && w.role === "owner");
}

/** Review access: the viewer's items to decide. */
export function AccessReviewInbox({ session }: { session: Session }) {
  const ask = useAction();
  const decide = (i: InboxItem, decision: "keep" | "flag_idp" | "remove") => ask({
    title: decision === "remove" ? `${isKey(i) ? "Revoke" : "Remove"} ${removalTarget(i)}?` : decision === "flag_idp" ? `Flag ${personText(i)}'s access for the identity provider?` : isKey(i) ? `Keep ${removalTarget(i)}?` : `Keep ${personText(i)} as ${accessText(i)}?`,
    danger: decision === "remove",
    description: decision === "remove" ? removalConsequence(i) : decision === "flag_idp" ? "This access comes from an identity provider group: it is fixed there, not here. The flag records your decision for the review." : isKey(i) ? `${keyIdentity(i)} This key stays as it is.` : `${personText(i)} keeps this access.`,
    submitLabel: decision === "remove" ? isKey(i) ? "Revoke key" : "Remove access" : decision === "flag_idp" ? "Flag" : "Keep",
    successNotice: decision === "remove" ? isKey(i) ? "Key revoked." : "Access removed." : decision === "flag_idp" ? "Flagged." : "Kept.",
    run: (_, signal) => api<ReviewItem>(decisionPath(i.id), { method: "POST", body: { decision }, signal }),
  });
  const columns: DataTableColumn<InboxItem>[] = [
    personColumn<InboxItem>(),
    accessColumn<InboxItem>(),
    { id: "review", header: "Review", accessor: i => i.campaign_name, cell: i => <CellText primary={i.campaign_name} secondary={`Due ${new Date(i.due_at).toLocaleDateString()}`} /> },
    { id: "decision", header: "Decision", accessor: i => i.decision ?? "", cell: i => <DecisionCell item={i} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Review access" description="Access you are asked to review: memberships and keys of the Teams and Projects you own or administer (never your own keys), platform roles if you are a Platform Admin, and anything assigned to you. Keep it, remove it, or flag access from an identity provider group. Removing a key revokes it." />
    <DirectoryTable<InboxItem> path={inboxPath} label="Access to review" storageKey="access-review-inbox" columns={columns} rowKey={i => i.id} rowLabel={itemLabel}
      rowActions={i => [
        { label: "Keep…", onSelect: () => decide(i, "keep") },
        { label: "Flag for identity provider…", hidden: i.source !== "group", onSelect: () => decide(i, "flag_idp") },
        { label: isKey(i) ? "Revoke…" : "Remove…", danger: true, hidden: !mayRemove(session, i), onSelect: () => decide(i, "remove") },
      ]}
      empty={{ icon: <ClipboardCheck />, title: "Nothing to review.", description: "Access reviews you take part in appear here." }} />
  </Stack>;
}

const workspaceField: Field = {
  name: "workspace_ids", label: "Only these Teams and Projects", type: "search", hint: "Optional", placeholder: "Search teams and projects", maxSelections: 50,
  help: "Leave empty to review every Team and Project. Applies to memberships and keys.",
  visibleWhen: v => narrowable(parseIds(v.subjects)),
  search: { path: `${platformPath}/workspaces?status=active`, multiple: true, emptyText: "No Team or Project matches.", option: ((w: DirectoryWorkspace) => ({ value: w.id, label: w.name })) as never },
};
const narrowable = (subjects: string[]) => subjects.includes("workspace_memberships" satisfies ReviewSubject) || subjects.includes("workspace_keys" satisfies ReviewSubject);
function parseIds(value: string | undefined): string[] {
  try { const ids = JSON.parse(value || "[]"); return Array.isArray(ids) ? ids.filter((v): v is string => typeof v === "string") : []; } catch { return []; }
}

/** Admin › Access reviews: campaigns, newest first; Platform Admins start one. */
export function PlatformAccessReviewsPage({ session }: { session: Session }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const start = () => ask({
    title: "Start an access review", description: "Takes a snapshot of the access below now. Reviewers decide each item; removals apply at once.",
    fields: [nameField,
      { name: "subjects", label: "What to review", type: "checkboxes", required: true, value: JSON.stringify(["workspace_memberships"]), options: Object.entries(subjectLabels).map(([value, label]) => ({ value, label })) },
      workspaceField,
      { name: "due_in_days", label: "Due in (days)", type: "number", value: "14", required: true, min: 1, max: 90 }],
    submitLabel: "Start review", successNotice: "Access review started.",
    run: (v, signal) => {
      const subjects = parseIds(v.subjects), ids = narrowable(subjects) ? parseIds(v.workspace_ids) : [];
      return api<ReviewCampaignDetail>(reviewsPath, { method: "POST", body: { name: v.name, subjects, workspace_ids: ids, due_in_days: Number(v.due_in_days) }, signal });
    },
  });
  const columns: DataTableColumn<ReviewCampaign>[] = [
    { id: "name", header: "Review", accessor: c => c.name, rowHeader: true, hideable: false, cell: c => <ResourceLink search={{ page: "access-review-detail", record: c.id }}>{c.name}</ResourceLink> },
    { id: "scope", header: "Covers", accessor: c => c.subjects.join(","), cell: c => <CellText primary={subjectsText(c.subjects)} secondary={c.workspace_ids.length ? `${c.workspace_ids.length} selected workspaces` : undefined} /> },
    { id: "items", header: "Items", accessor: c => c.item_count, cell: c => c.item_count },
    { id: "due", header: "Due", accessor: c => c.due_at, cell: c => <span className={s.secondary}>{new Date(c.due_at).toLocaleDateString()}</span> },
    { id: "state", header: "Status", accessor: c => campaignState(c), cell: c => <StatusBadge tone={campaignStateTone[campaignState(c)]}>{campaignStateLabels[campaignState(c)]}</StatusBadge> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Access reviews" description="One-off reviews of who has access to Teams and Projects, of their keys, and of Admin and Auditor roles. Workspace owners and admins review their workspaces and their keys; Platform Admins review platform roles." actions={writable && <Button onClick={start}><Plus aria-hidden />Start review</Button>} />
    <DirectoryTable<ReviewCampaign> path={reviewsPath} label="Access reviews" storageKey="access-reviews" columns={columns} rowKey={c => c.id} rowLabel={c => c.name}
      empty={{ icon: <ClipboardCheck />, title: "No access reviews yet.", description: writable ? "Start one to have owners and admins confirm who should keep access." : "Platform Admins start access reviews." }} />
    <AccessReviewSchedules session={session} />
  </Stack>;
}

/** The calendar fields of a schedule, with its current values when changing one. */
function calendarFields(current?: ReviewSchedule): Field[] {
  return [
    { name: "cadence", label: "Repeats", type: "select", required: true, value: current?.cadence ?? "monthly", options: Object.entries(cadenceLabels).map(([value, label]) => ({ value, label })) },
    { name: "start_day", label: "Start day", type: "number", required: true, min: 1, max: 31, value: String(current?.start_day ?? 1), help: "Each review starts at 00:00 UTC on this day of the month, or on the month's last day when it is shorter." },
    { name: "start_month", label: "Start month", type: "select", value: String(current?.start_month ?? 1), options: monthNames.map((m, i) => ({ value: String(i + 1), label: m })),
      visibleWhen: (v: Values) => v.cadence !== "monthly", requiredWhen: (v: Values) => v.cadence !== "monthly", help: "Quarterly: a month of the cycle (every third month from it). Yearly: the month." },
  ];
}
const openDaysField = (value = "14"): Field => ({ name: "open_days", label: "Each review is due after (days)", type: "number", value, required: true, min: 1, max: 90 });

/**
 * Schedules that start an ordinary review each period. A run while the
 * schedule's previous review is still open is skipped (Platform Admins get
 * an alert) and is never queued.
 */
function AccessReviewSchedules({ session }: { session: Session }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const create = () => ask({
    title: "New review schedule", description: "Starts a review like the one below each period. The first one starts at the next start day, never today.",
    fields: [{ ...nameField, maxLength: 100, help: "Each review is named after it, with the date it starts." },
      { name: "subjects", label: "What to review", type: "checkboxes", required: true, value: JSON.stringify(["workspace_memberships"]), options: Object.entries(subjectLabels).map(([value, label]) => ({ value, label })) },
      workspaceField, openDaysField(), ...calendarFields()],
    submitLabel: "Create schedule", successNotice: "Review schedule created.",
    run: (v, signal) => {
      const subjects = parseIds(v.subjects), ids = narrowable(subjects) ? parseIds(v.workspace_ids) : [];
      return api<ReviewSchedule>(schedulesPath, { method: "POST", body: { name: v.name, subjects, workspace_ids: ids, open_days: Number(v.open_days), ...calendarBody(v) }, signal });
    },
  });
  const change = (x: ReviewSchedule) => ask({
    title: `Change ${x.name}`, description: "Applies to reviews it starts from now on; reviews already started are unchanged. A new calendar starts at its next start day.",
    fields: [{ ...nameField, maxLength: 100, value: x.name }, openDaysField(String(x.open_days)), ...calendarFields(x)],
    submitLabel: "Save", successNotice: "Review schedule changed.",
    run: (v, signal) => api<ReviewSchedule>(schedulePath(x.id), { method: "PATCH", body: { name: v.name, open_days: Number(v.open_days), ...calendarBody(v) }, signal }),
  });
  const pause = (x: ReviewSchedule, paused: boolean) => ask({
    title: paused ? `Pause ${x.name}?` : `Resume ${x.name}?`,
    description: paused ? "No review starts while it is paused. Reviews already started are unchanged." : "The next review starts at the next start day; periods missed while it was paused do not run.",
    submitLabel: paused ? "Pause" : "Resume", successNotice: paused ? "Review schedule paused." : "Review schedule resumed.",
    run: (_, signal) => api<ReviewSchedule>(schedulePath(x.id), { method: "PATCH", body: { paused }, signal }),
  });
  const remove = (x: ReviewSchedule) => ask({
    title: `Delete ${x.name}?`, danger: true, description: "No further reviews start. Reviews it already started stay as they are.",
    submitLabel: "Delete schedule", successNotice: "Review schedule deleted.",
    run: (_, signal) => api(schedulePath(x.id), { method: "DELETE", signal }),
  });
  const columns: DataTableColumn<ReviewSchedule>[] = [
    { id: "name", header: "Schedule", accessor: x => x.name, rowHeader: true, hideable: false, cell: x => <CellText primary={x.name} secondary={`${subjectsText(x.subjects)}${x.workspace_ids.length ? ` · ${x.workspace_ids.length} selected workspaces` : ""}`} /> },
    { id: "repeats", header: "Repeats", accessor: x => x.cadence, cell: x => <CellText primary={cadenceText(x)} secondary={`Due after ${x.open_days} days`} /> },
    { id: "next", header: "Next start", accessor: x => x.next_run_at ?? "", cell: x => x.next_run_at ? <span className={s.secondary}>{utcDate(x.next_run_at)}</span> : <StatusBadge tone="neutral">Paused</StatusBadge> },
    { id: "last", header: "Last run", accessor: x => x.last_run?.period_at ?? "", cell: x => <CellText primary={lastRunText(x.last_run)} secondary={x.last_run ? utcDate(x.last_run.period_at) : undefined} /> },
    { id: "review", header: "Last review", accessor: x => x.last_campaign?.created_at ?? "", cell: x => x.last_campaign ? <ResourceLink search={{ page: "access-review-detail", record: x.last_campaign.id }}>{x.last_campaign.name}</ResourceLink> : <span className={s.muted}>None yet</span> },
  ];
  return <>
    <Heading title="Schedules" description="Reviews that start on their own each month, quarter or year, in UTC. A review is skipped (Platform Admins are alerted) while the schedule's previous one is still open." actions={writable && <Button variant="secondary" onClick={create}><Plus aria-hidden />New schedule</Button>} />
    <DirectoryTable<ReviewSchedule> path={schedulesPath} label="Review schedules" storageKey="access-review-schedules" columns={columns} rowKey={x => x.id} rowLabel={x => x.name}
      rowActions={x => !writable ? [] : [
        { label: "Change…", onSelect: () => change(x) },
        { label: "Pause…", hidden: scheduleState(x) === "paused", onSelect: () => pause(x, true) },
        { label: "Resume…", hidden: scheduleState(x) === "active", onSelect: () => pause(x, false) },
        { label: "Delete…", danger: true, onSelect: () => remove(x) },
      ]}
      empty={{ icon: <CalendarClock />, title: "No review schedules.", description: writable ? "Create one to start a review every month, quarter or year." : "Platform Admins create review schedules." }} />
  </>;
}

/** One campaign: counts, items with their decision and reviewer. */
export function AccessReviewDetail({ session, id }: { session: Session; id: string }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const q = useApi<ReviewCampaignDetail>(reviewPath(id), session.capabilities.platform_read);
  useResourceName(q.data?.name);
  if (q.isPending) return <p role="status">Loading access review…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id) return <ErrorNotice error={new Error("The returned review does not match the requested record.")} />;
  const c = q.data, state = campaignState(c), open = state !== "closed", n = c.counts;
  const close = () => ask({ title: `Close ${c.name}?`, description: "Reviewers can no longer decide its items. Decisions already made stay.", submitLabel: "Close review", successNotice: "Access review closed.", run: (_, signal) => api(reviewClosePath(id), { method: "POST", signal }) });
  const assign = (i: ReviewItem) => ask({
    title: `Choose who reviews ${personText(i)} (${accessText(i)})`, description: i.subject_kind === "platform_role" ? "A Platform Admin, other than the person under review." : `An owner or admin of ${i.workspace_name ?? "the workspace"}, or a Platform Admin, other than the person under review.`,
    fields: [{ name: "user_id", label: "Reviewer", type: "search", required: true, placeholder: "name@example.com", search: { path: `${platformPath}/users?status=active&role=${i.subject_kind === "platform_role" ? "admin" : "any"}`, option: ((u: DirectoryUser) => u.platform_role && u.email && u.id !== i.user_id ? { value: u.id, label: u.email } : undefined) as never } }],
    submitLabel: "Assign", successNotice: "Reviewer assigned.",
    run: (v, signal) => api<ReviewItem>(reviewerPath(id, i.id), { method: "POST", body: { user_id: v.user_id }, signal }),
  });
  const unassign = (i: ReviewItem) => ask({ title: "Back to the automatic reviewers?", description: "The workspace's owners and admins (or, for platform roles, the Platform Admins) review it again.", submitLabel: "Reset", successNotice: "Reviewer reset.", run: (_, signal) => api<ReviewItem>(reviewerPath(id, i.id), { method: "POST", body: { user_id: null }, signal }) });
  const columns: DataTableColumn<ReviewItem>[] = [
    personColumn<ReviewItem>(),
    accessColumn<ReviewItem>(),
    { id: "decision", header: "Decision", accessor: i => i.decision ?? "", cell: i => <DecisionCell item={i} /> },
    { id: "reviewer", header: "Reviewer", accessor: i => i.active && !i.reviewable ? "none" : i.reviewer_user_id ?? "", cell: i => i.active && !i.reviewable ? <StatusBadge tone="warning">No reviewer</StatusBadge> : i.reviewer_user_id ? <span title="An assignee who can no longer review it is ignored: the automatic reviewers decide again.">Assigned</span> : isKey(i) ? <span className={s.muted} title="The workspace's owners and admins review its keys, never their own. A key held by the only owner or admin stays undecided until the review is closed.">Owners and admins</span> : <span className={s.muted}>Automatic</span> },
  ];
  const summary = `${n.by_decision.pending} undecided · ${n.by_decision.keep} kept · ${n.by_decision.remove} removed${n.by_kind.workspace_key ? " or revoked" : ""} · ${n.by_decision.flag_idp} flagged${n.without_reviewer ? ` · ${n.without_reviewer} without a reviewer` : ""}${n.inactive ? ` · ${n.inactive} no longer in effect` : ""}`;
  return <Stack gap={6} className={s.page}>
    <Heading title={c.name} description={`${subjectsText(c.subjects)} · ${c.item_count} items · due ${new Date(c.due_at).toLocaleDateString()} · ${summary}${c.schedule_id ? " · started by a schedule" : ""}`}
      actions={<Inline gap={2}><StatusBadge tone={campaignStateTone[state]}>{campaignStateLabels[state]}</StatusBadge><TextLink href={reviewExportPath(id)} download>Export CSV</TextLink>{writable && open && <Button variant="secondary" onClick={close}>Close review</Button>}</Inline>} />
    <DirectoryTable<ReviewItem> path={reviewItemsPath(id)} label="Items" storageKey="access-review-items" columns={columns} rowKey={i => i.id} rowLabel={itemLabel}
      rowActions={i => !writable || !open || isKey(i) || i.user_id === session.user.id ? [] : [
        { label: "Assign reviewer…", hidden: !i.active, onSelect: () => assign(i) },
        { label: "Back to automatic reviewers…", hidden: !i.reviewer_user_id, onSelect: () => unassign(i) },
      ]}
      empty={{ icon: <ClipboardCheck />, title: "No items.", description: "" }} />
  </Stack>;
}
