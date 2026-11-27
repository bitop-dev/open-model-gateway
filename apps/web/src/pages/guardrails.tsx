/*
 * Guardrails pages (#840, docs/guardrails-design.md §5 "Dashboard"):
 * - Admin › Settings › Guardrails: the type default of each workspace kind
 *   (Platform Admins write, Auditors read) and what this release runs.
 * - Admin › Team/Project › Guardrails: the platform override that replaces
 *   the type default (reset restores it), the workspace's own rules
 *   read-only, and this month's decision totals.
 * - Workspace › Settings › Guardrails: the rules in effect with their
 *   provenance and what each inspects, and the workspace's own rules (its
 *   owners and admins, and a personal workspace's owner, edit them; they can
 *   only add rules or make the platform's stricter).
 * Totals are counts the server records; nothing is estimated here.
 */
import { api, type Session, type Workspace } from "../lib/api";
import {
  detectorsPath, guardrailsPath, inspectedSentence, isTightening, platformGuardrailsPath, stricterPart, toInput, typeDefaultPath, workloadLabels,
  type Detectors, type PlatformWorkspaceGuardrails, type RuleSet, type StoredRule, type TypeDefault, type WorkspaceGuardrails, type WorkspaceKind,
} from "../lib/guardrails";
import { kindLabels } from "../lib/people";
import { ResourcePage } from "../components/resource-page";
import { EffectiveRules, LayerEditor, guardrailWrite, putPlatformLayer, tightenRule, unchanged } from "../components/guardrails";
import { Alert, Button, ErrorNotice, Stack, useAction, useApi } from "../components/ui";
import { Badge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { Table, Td, Tr } from "../components/ui/table/table";
import s from "./shared.module.css";

const kinds: WorkspaceKind[] = ["team", "project", "personal"];
const actionNames: Record<string, string> = { monitored: "Monitored", redacted: "Redacted", blocked: "Blocked", stopped: "Stopped", detector_failed: "Could not check", skipped: "Not readable" };
/** Outcomes that count matched values; the others (could not check, not readable) have no findings. */
const matching = new Set(["monitored", "redacted", "blocked", "stopped"]);
const categoryNames: Record<string, string> = { secret: "Secrets", custom: "Custom patterns", personal_data: "Personal data", moderation: "Moderation", prompt_injection: "Prompt injection" };

/** Admin › Settings › Guardrails. */
export function AdminGuardrails({ session, tab, onTabChange }: { session: Session; tab?: string; onTabChange: (tab: string) => void }) {
  return <ResourcePage title="Guardrails" description="Rules that check request text before it reaches a provider. Each workspace kind has a default; a workspace can only add rules or make them stricter."
    tab={tab} onTabChange={onTabChange} tabs={[
      ...kinds.map(kind => ({ value: kind, label: `${kindLabels[kind]} default`, content: <TypeDefaultTab key={kind} kind={kind} writable={session.capabilities.platform_write} /> })),
      { value: "detectors", label: "What runs", content: <DetectorsCard /> },
    ]} />;
}

function TypeDefaultTab({ kind, writable }: { kind: WorkspaceKind; writable: boolean }) {
  const path = typeDefaultPath(kind), q = useApi<TypeDefault>(path);
  if (q.isPending) return <p role="status">Loading guardrails…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const set = q.data.rule_set, localOnly = !!set?.local_only, every = `every ${kindLabels[kind].toLowerCase()} workspace without a platform override`;
  // Each write reads the layer again and refuses if it changed since the page
  // loaded it, so a stale page never undoes another admin's change.
  const fresh = async (signal: AbortSignal) => { const now = (await api<TypeDefault>(path, { signal })).rule_set; unchanged(now, set); return now; };
  return <Stack gap={6}>
    <Alert tone="info" title="What is checked">{inspectedSentence(q.data.inspected_workloads)}</Alert>
    <LayerEditor title={`${kindLabels[kind]} default`} description={`Applies to ${every}.`} set={set} writable={writable}
      reach={{ change: `This applies to ${every}.`, remove: `It stops applying to ${every}, and their tightenings of it stop applying.` }}
      save={async (rules, signal) => putPlatformLayer(path, !!(await fresh(signal))?.local_only, rules, signal)} localOnly={localOnly}
      onLocalOnly={async (value, signal) => putPlatformLayer(path, value, ((await fresh(signal))?.rules ?? []).map(toInput), signal)} empty="No default rules." />
  </Stack>;
}

function DetectorsCard() {
  const q = useApi<Detectors>(detectorsPath);
  if (q.isPending) return <p role="status">Loading detectors…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const names: Record<string, string> = { pattern: "Patterns (built-in secret shapes and custom patterns)", systemone: "System One", llm_judge: "Model judge", external: "External service" };
  return <Card title="Detectors" titleAs="h2" description={inspectedSentence(q.data.inspected_workloads)} flush>
    <Table caption="Detectors" stack columns={["Detector", "Where text goes", "This release"]}>{q.data.detectors.map(d => <Tr key={d.kind}>
      <Td><span className={s.primary}>{names[d.kind] ?? d.kind}</span>{d.available && <span className={s.secondary}>{d.workloads.map(w => workloadLabels[w] ?? w).join(", ")}</span>}</Td>
      <Td>{d.egress === "local" ? "Stays inside the gateway" : d.egress === "model" ? "Sent to a model" : "Sent to a service"}</Td>
      <Td>{d.available ? <Badge size="sm" tone="success">Available</Badge> : <Badge size="sm">Later release</Badge>}</Td>
    </Tr>)}</Table>
  </Card>;
}

/** The live platform layer's rules by id: what the workspace's tightenings are checked against. */
const platformRules = (g: WorkspaceGuardrails) => new Map((g.platform.rule_set?.rules ?? []).filter((e): e is StoredRule => !isTightening(e)).map(r => [r.id, r]));
/**
 * The local layer's entries as the server will accept them against the live
 * platform layer: a tightening whose platform rule is gone is left out, and
 * one keeps only the settings not weaker than its rule (left out when none
 * is; the server refuses a weaker one as loosening).
 */
function liveEntries(rules: Record<string, unknown>[], g: WorkspaceGuardrails): Record<string, unknown>[] {
  const base = platformRules(g);
  return rules.flatMap(r => {
    if (typeof r.tightens !== "string") return [r];
    const rule = base.get(r.tightens), part = rule ? stricterPart(r, rule) : null;
    return part ? [part] : [];
  });
}
/** Tightenings of the page's local layer that no longer apply in full, with what the next save does to each. */
function tighteningMarks(g: WorkspaceGuardrails): Map<string, string> {
  const base = platformRules(g), marks = new Map<string, string>();
  for (const e of g.local?.rules ?? []) {
    if (!isTightening(e)) continue;
    const rule = base.get(e.tightens), stored = toInput(e), part = rule ? stricterPart(stored, rule) : null;
    if (!rule) marks.set(e.id, "Its platform rule was removed: it no longer applies, and the next save of this layer removes it.");
    else if (!part) marks.set(e.id, `“${rule.name}” is now stricter than this: it no longer applies, and the next save of this layer removes it.`);
    else if (Object.keys(part).length < Object.keys(stored).length) marks.set(e.id, `“${rule.name}” is now stricter than part of this: the next save of this layer keeps only the rest.`);
  }
  return marks;
}

/** Workspace › Settings › Guardrails. */
export function WorkspaceGuardrailsTab({ workspace }: { workspace: Workspace }) {
  const path = guardrailsPath(workspace.id), q = useApi<WorkspaceGuardrails>(path), ask = useAction();
  if (q.isPending) return <p role="status">Loading guardrails…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const g = q.data, marks = tighteningMarks(g), names = Object.fromEntries([...platformRules(g)].map(([id, r]) => [id, r.name]));
  // Each save reads the workspace again: it refuses if this layer changed
  // since the page loaded it (a stale page never undoes another admin's
  // change), and checks the tightenings against the live platform layer, so
  // a page loaded before that layer changed never drops one that applies
  // again nor sends one the server refuses.
  const save = async (rules: Record<string, unknown>[], signal: AbortSignal) => {
    const now = await api<WorkspaceGuardrails>(path, { signal });
    unchanged(now.local, g.local);
    return api(path, { method: "PUT", body: { rules: liveEntries(rules, now) }, signal });
  };
  return <Stack gap={6}>
    <Alert tone="info" title="What is checked">{inspectedSentence(g.inspected_workloads)}{g.egress.local_only ? " Text never leaves the gateway to be checked." : ""}</Alert>
    <Card title="Rules in effect" titleAs="h2" description={g.platform.source === "workspace_override" ? "Set by a Platform Admin for this workspace, with this workspace's own rules." : `The ${kindLabels[g.kind].toLowerCase()} default, with this workspace's own rules.`} flush={g.effective.length > 0}>
      <EffectiveRules caption={`Guardrails in effect for ${workspace.name}`} entries={g.effective} tighten={g.can_edit ? e => tightenRule(ask, e, g.local, save, names) : undefined} />
    </Card>
    {(g.can_edit || (g.local?.rules.length ?? 0) > 0) && <LayerEditor title="This workspace's rules" description={g.can_edit ? "Add rules or make the platform's stricter. They can't turn a platform rule off." : "Set by this workspace's admins."} set={g.local} writable={g.can_edit} save={save} marks={marks} names={names} empty="No rules of this workspace's own." />}
  </Stack>;
}

/** Admin › Team/Project › Guardrails: the override, the type default it replaces and this month's totals. */
export function WorkspaceGuardrailsAdmin({ id, writable }: { id: string; writable: boolean }) {
  const path = platformGuardrailsPath(id), q = useApi<PlatformWorkspaceGuardrails>(path), ask = useAction();
  if (q.isPending) return <p role="status">Loading guardrails…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const g = q.data, override = g.override, localOnly = !!override?.local_only;
  // A tightening names the platform rule it tightens: the override's rules are
  // new rules, so tightenings of the rules it replaces stop applying.
  const dropped = (replaced: RuleSet | null, rules: string) => {
    const ids = new Set((replaced?.rules ?? []).map(r => r.id));
    const n = g.local_visible ? (g.local?.rules ?? []).filter(e => isTightening(e) && ids.has(e.tightens)).length : null;
    if (n === 0 || ids.size === 0) return "";
    return ` ${n === null ? `Any tightening this workspace made of the ${rules} stops applying` : `This workspace's ${n === 1 ? "tightening" : `${n} tightenings`} of the ${rules} stop${n === 1 ? "s" : ""} applying`}; it can tighten the rules that replace them.`;
  };
  // Each write reads the workspace again and refuses if the override changed
  // since the page loaded it; a new override starts from the default as it is now.
  const fresh = async (signal: AbortSignal) => { const now = await api<PlatformWorkspaceGuardrails>(path, { signal }); unchanged(now.override, override); return now; };
  const create = () => ask({ title: "Override the default for this workspace?", description: `Starts from a copy of the ${kindLabels[g.kind].toLowerCase()} default's rules. While it exists, the default no longer applies here.${dropped(g.type_default, "default's rules")}`, submitLabel: "Create override", successNotice: "Override created.",
    run: (_, signal) => guardrailWrite(async () => { const now = await fresh(signal); return putPlatformLayer(path, !!now.type_default?.local_only, (now.type_default?.rules ?? []).map(r => { const { id: _id, ...rest } = toInput(r); return rest; }), signal); }) });
  const reset = () => ask({ title: "Reset to the default?", description: `Removes this workspace's override; the ${kindLabels[g.kind].toLowerCase()} default applies again.${dropped(override, "override's rules")}`, danger: true, submitLabel: "Reset", successNotice: "Default restored.", run: async (_, signal) => { await fresh(signal); return api(path, { method: "DELETE", signal }); } });
  return <Stack gap={6}>
    <Alert tone="info" title="What is checked">{inspectedSentence(g.inspected_workloads)}</Alert>
    {override
      ? <LayerEditor title="Platform override" description={`Replaces the ${kindLabels[g.kind].toLowerCase()} default for this workspace.`} set={override} writable={writable}
          reach={{ change: "This applies to this workspace only.", remove: "This workspace's tightenings of it stop applying." }}
          save={async (rules, signal) => putPlatformLayer(path, !!(await fresh(signal)).override?.local_only, rules, signal)} localOnly={localOnly}
          onLocalOnly={async (value, signal) => putPlatformLayer(path, value, ((await fresh(signal)).override?.rules ?? []).map(toInput), signal)} />
      : <LayerEditor title={`${kindLabels[g.kind]} default`} description="Applies here: this workspace has no override. Edit it under Settings › Guardrails." set={g.type_default} writable={false} save={() => Promise.resolve()} empty="No default rules." />}
    {writable && <div><Button size="sm" variant={override ? "ghost" : "secondary"} onClick={override ? reset : create}>{override ? "Reset to default" : "Override the default"}</Button></div>}
    {g.local_visible && g.local && <LayerEditor title="This workspace's rules" description="Set by its owners and admins." set={g.local} writable={false} save={() => Promise.resolve()} empty="None." />}
    <Card title="Decisions this month" titleAs="h2" description="Counts recorded by the gateway (UTC month). Findings are distinct matched values; no text is kept. A request refused because no check could run then (the gateway was busy) writes no row and is not counted here." flush={g.decisions_this_month.length > 0}>
      {g.decisions_this_month.length === 0 ? <p className={s.note}>No decisions this month.</p> :
        <Table caption="Guardrail decisions this month" columns={["Outcome", "Looked for", { label: "Decisions", numeric: true }, { label: "Findings", numeric: true }]}>{g.decisions_this_month.map(t => <Tr key={`${t.action}:${t.category}`}>
          <Td>{actionNames[t.action] ?? t.action}</Td><Td>{categoryNames[t.category] ?? t.category}</Td><Td>{t.decisions}</Td><Td>{matching.has(t.action) ? t.findings : "—"}</Td>
        </Tr>)}</Table>}
    </Card>
  </Stack>;
}
