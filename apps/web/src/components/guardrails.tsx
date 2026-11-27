/*
 * Guardrail rule layers (#840, docs/guardrails-design.md §5 "Dashboard"):
 * the rules in effect with their provenance and one sentence on what each
 * inspects and where it goes, and the editor of one layer. A PUT replaces
 * the whole layer, so every edit sends the layer's entries again. Patterns
 * are shown only to the layer's writers and platform staff (the server
 * returns `patterns` to them and a count to everyone else); matched text is
 * never shown anywhere.
 */
import { api, ApiError } from "../lib/api";
import {
  actionHelp, actionLabels, inactiveLabels, inspectionSentence, isTightening, protectionLabels, refusalMessages, ruleInput, sourceLabels, stricterActions, toInput,
  type EffectiveEntry, type Entry, type InputAction, type RuleSet, type StoredRule, type Tightening,
} from "../lib/guardrails";
import type { Field } from "../lib/forms";
import { Button, Inline, useAction } from "./ui";
import { Badge } from "./ui/badge/badge";
import { Card } from "./ui/card/card";
import { EmptyState } from "./ui/empty-state/empty-state";
import { Table, Td, Tr } from "./ui/table/table";
import { ShieldCheck } from "lucide-react";
import s from "../pages/shared.module.css";

/** Runs a guardrail write, with plain messages for its refusal reasons. */
export async function guardrailWrite<T>(write: () => Promise<T>): Promise<T> {
  try { return await write(); } catch (error) {
    if (error instanceof ApiError && error.reason && refusalMessages[error.reason]) throw new ApiError(error.status, error.code, refusalMessages[error.reason]!, error.reason);
    throw error;
  }
}

const actionTone = (a?: InputAction) => a === "block" ? "danger" as const : a === "monitor" ? "info" as const : "neutral" as const;
export function ActionBadge({ action }: { action?: InputAction }) { return <Badge size="sm" tone={actionTone(action)}>{actionLabels[action ?? "off"]}</Badge>; }

/** The rules in effect, each with its provenance, status and sentence. */
export function EffectiveRules({ entries, caption, tighten }: { entries: EffectiveEntry[]; caption: string; /** Local layer writers: tighten a platform rule. */ tighten?: (e: EffectiveEntry) => void }) {
  const rules = entries;
  if (rules.length === 0) return <EmptyState size="compact" icon={<ShieldCheck />} title="No guardrail rules." description="Requests are not inspected." />;
  return <Table caption={caption} stack columns={["Rule", "Set by", "On input", "Status", ...(tighten ? [""] : [])]}>{rules.map(e => {
    const dangling = e.inactive_reason === "tightened_rule_missing";
    return <Tr key={e.id}>
      <Td><span className={s.primary}>{dangling ? "Tightening of a removed rule" : e.name}</span>{!dangling && <span className={s.secondary}>{inspectionSentence(e)}</span>}</Td>
      <Td><Badge size="sm" variant="outline">{sourceLabels[e.source]}</Badge>{e.tightened_by?.length ? <span className={s.secondary}>Tightened by this workspace</span> : null}</Td>
      <Td>{dangling ? "—" : <ActionBadge action={e.input?.action} />}</Td>
      <Td>{e.active ? <Badge size="sm" tone="success">Active</Badge> : <Badge size="sm" tone="warning">{e.inactive_reason ? inactiveLabels[e.inactive_reason] : "Off"}</Badge>}</Td>
      {tighten && <Td>{canTighten(e) && <Button size="sm" variant="ghost" aria-label={`Tighten ${e.name}`} onClick={() => tighten(e)}>Tighten</Button>}</Td>}
    </Tr>;
  })}</Table>;
}

/** A platform rule in effect that a local entry can still make stricter: a stricter action, or refusing when its check can't run. */
const canTighten = (e: EffectiveEntry) => e.source !== "workspace_local" && e.active && (e.input?.action !== "block" || e.on_error !== "fail_closed");

/** The server's `on_error` for a rule that names none. */
const defaultOnError = (action: InputAction) => action === "block" ? "fail_closed" : "fail_open";

const ruleFields = (rule?: StoredRule, actions: InputAction[] = ["monitor", "block"]): Field[] => [
  { name: "name", label: "Name", required: true, maxLength: 100, value: rule?.name ?? "" },
  { name: "protection", label: "Looks for", type: "select", required: true, value: rule?.protection ?? "secrets", options: [{ value: "secrets", label: "Secrets (keys, tokens, private keys, connection strings)" }, { value: "custom", label: "Custom patterns" }] },
  { name: "patterns", label: "Patterns", type: "textarea", value: rule?.detector.patterns?.join("\n") ?? "", visibleWhen: v => v.protection === "custom", requiredWhen: v => v.protection === "custom", help: "Regular expressions, one per line: at most 50 per layer, 512 bytes each. Use ASCII classes like [0-9].", validate: (v, all) => all.protection !== "custom" ? undefined : v.split("\n").filter(p => p.trim()).length > 50 ? "Use at most 50 patterns." : undefined },
  { name: "action", label: "On input", type: "select", required: true, value: rule?.input.action ?? "monitor", options: actions.map(a => ({ value: a, label: `${actionLabels[a]}: ${actionHelp[a]}` })) },
  // A stored value that is its action's default shows as the default, so changing the action changes it too.
  { name: "on_error", label: "If the check can't run", type: "select", value: rule?.on_error && rule.on_error !== defaultOnError(rule.input.action) ? rule.on_error : "", help: "When the gateway can't check a request (it's busy, or a pattern takes too long).", options: [{ value: "", label: "Default: refused for Block, goes on for Monitor" }, { value: "fail_closed", label: "Refuse the request" }, { value: "fail_open", label: "Let the request go on and record it" }] },
  { name: "workloads", label: "Requests", type: "checkboxes", required: true, value: JSON.stringify(rule?.workloads ?? ["generation", "embeddings"]), options: [{ value: "generation", label: "Generation (chat, responses, messages)" }, { value: "embeddings", label: "Embeddings" }] },
];

/**
 * One layer's own entries. `save` receives the layer's full entry list.
 * Platform layers also carry `local_only` (kept text inside the gateway).
 */
export function LayerEditor({ title, description, set, writable, save, localOnly, onLocalOnly, empty, stale }: {
  title: string; description?: string; set: RuleSet | null; writable: boolean; save: (rules: Record<string, unknown>[], signal: AbortSignal) => Promise<unknown>;
  /** Platform layers: whether rules must keep text inside the gateway. */ localOnly?: boolean; onLocalOnly?: (value: boolean, signal: AbortSignal) => Promise<unknown>; empty?: string;
  /** Local layers: tightenings whose platform rule is gone (the server refuses them on save, so `save` leaves them out). */ stale?: ReadonlySet<string>;
}) {
  const ask = useAction(), entries = set?.rules ?? [], all = () => entries.map(toInput);
  const add = () => ask({ title: `Add a rule to ${title.toLowerCase()}`, fields: ruleFields(), submitLabel: "Add rule", successNotice: "Rule added.", run: (v, signal) => guardrailWrite(() => save([...all(), ruleInput(v)], signal)) });
  const edit = (rule: StoredRule) => ask({ title: `Edit ${rule.name}`, fields: ruleFields(rule, rule.input.action === "off" ? ["off", "monitor", "block"] : ["monitor", "block"]), submitLabel: "Save rule", successNotice: "Rule saved.", run: (v, signal) => guardrailWrite(() => save(entries.map(e => e.id === rule.id ? ruleInput(v, toInput(rule)) : toInput(e)), signal)) });
  const remove = (e: Entry) => ask({ title: isTightening(e) ? "Remove this tightening?" : `Remove ${(e as StoredRule).name}?`, description: isTightening(e) ? "The platform rule runs as the platform set it." : "Requests are no longer checked by this rule.", danger: true, submitLabel: "Remove", successNotice: "Rule removed.", run: (_, signal) => guardrailWrite(() => save(entries.filter(x => x.id !== e.id).map(toInput), signal)) });
  const local = () => ask({ title: "Keep text inside the gateway?", description: "When on, no rule of this layer or of a workspace's own rules may send text out of the gateway to be checked. Every rule in this release checks text inside the gateway.", fields: [{ name: "local_only", label: "Keep text inside the gateway", type: "switch", value: String(!!localOnly) }], submitLabel: "Save", run: (v, signal) => guardrailWrite(() => onLocalOnly!(v.local_only === "true", signal)) });
  return <Card title={title} titleAs="h2" description={description} flush={entries.length > 0}
    actions={writable && <Inline gap={2}>{onLocalOnly && <Button size="sm" variant="ghost" onClick={local}>Text stays inside: {localOnly ? "On" : "Off"}</Button>}<Button size="sm" variant="secondary" onClick={add}>Add rule</Button></Inline>}>
    {entries.length === 0 ? <EmptyState size="compact" icon={<ShieldCheck />} title={empty ?? "No rules."} description={writable ? "Add a rule to check requests." : undefined} /> :
      <Table caption={title} stack columns={["Rule", "On input", ...(writable ? [""] : [])]}>{entries.map(e => isTightening(e)
        ? <Tr key={e.id}><Td><span className={s.primary}>{stale?.has(e.id) ? "Tightens a removed platform rule" : "Tightens a platform rule"}</span><span className={s.secondary}>{stale?.has(e.id) ? "No longer applies; the next save of this layer removes it." : e.input?.action ? `On input: ${actionLabels[e.input.action]}` : "Refuses the request if the check can't run"}</span></Td><Td><ActionBadge action={e.input?.action} /></Td>{writable && <Td><Button size="sm" variant="ghost" aria-label="Remove tightening" onClick={() => remove(e)}>Remove</Button></Td>}</Tr>
        : <Tr key={e.id}><Td><span className={s.primary}>{e.name}</span><span className={s.secondary}>{protectionLabels[e.protection]} · {inspectionSentence(e)}</span></Td><Td><ActionBadge action={e.input.action} /></Td>{writable && <Td><Inline gap={1}><Button size="sm" variant="ghost" aria-label={`Edit ${e.name}`} onClick={() => edit(e)}>Edit</Button><Button size="sm" variant="ghost" aria-label={`Remove ${e.name}`} onClick={() => remove(e)}>Remove</Button></Inline></Td>}</Tr>)}
      </Table>}
  </Card>;
}

/**
 * "Tighten…": one platform rule made stricter by a tightening entry of the
 * local layer: a stricter input action, and refusing the request when the
 * check can't run. `rule` is the rule as it runs (already tightened), so the
 * choices start from what is in effect; the entry is written whole, keeping
 * any other setting of an existing tightening.
 */
export function tightenRule(ask: ReturnType<typeof useAction>, rule: EffectiveEntry, local: RuleSet | null, save: (rules: Record<string, unknown>[], signal: AbortSignal) => Promise<unknown>) {
  const current = rule.input?.action ?? "off", options = stricterActions(current);
  const others = (local?.rules ?? []).filter(e => !(isTightening(e) && e.tightens === rule.id));
  const existing = (local?.rules ?? []).find((e): e is Tightening => isTightening(e) && e.tightens === rule.id);
  const failOpen = rule.on_error !== "fail_closed";
  // Refusing when the check can't run is offered with Block: a Monitor rule lets requests go on.
  const refusing = (v: Record<string, string | undefined>) => failOpen && v.action === "block" && v.fail_closed === "true";
  ask({ title: `Tighten ${rule.name}`, description: "This workspace can make a platform rule stricter, never weaker.", fields: [
    { name: "action", label: "On input", type: "select", required: true, value: options[options.length - 1] ?? "block", options: options.map(a => ({ value: a, label: `${a === current ? "Keep " : ""}${actionLabels[a]}: ${actionHelp[a]}` })), validate: (value, all) => value === current && !refusing(all) ? "Choose a stricter action, or refuse the request when the check can't run." : undefined },
    ...(failOpen ? [{ name: "fail_closed", label: "Refuse the request if the check can't run", type: "switch" as const, value: "true", visibleWhen: (v: Record<string, string | undefined>) => v.action === "block" }] : []),
  ],
    submitLabel: "Tighten", successNotice: "Rule tightened.",
    run: (v, signal) => {
      // The entry states what is in effect after it, never less: the action
      // chosen (at least `current`), and refusing when the check can't run if
      // chosen now or already in effect through this entry. A stored setting
      // below the platform's current one would be refused as loosening.
      const { input: _input, on_error: storedError, ...kept } = existing ? toInput(existing) : {};
      const onError = refusing(v) || (!failOpen && storedError === "fail_closed") ? "fail_closed" : undefined;
      const entry = { ...kept, tightens: rule.id, input: { action: v.action }, ...(onError ? { on_error: onError } : {}) };
      return guardrailWrite(() => save([...others.map(toInput), entry], signal));
    } });
}

/** PUT a platform layer (type default or override): `local_only` is stated every time. */
export const putPlatformLayer = (path: string, localOnly: boolean, rules: Record<string, unknown>[], signal: AbortSignal) => api(path, { method: "PUT", body: { local_only: localOnly, rules }, signal });
