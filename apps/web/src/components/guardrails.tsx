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
  actionHelp, actionLabels, inactiveLabels, inspectionSentence, isTightening, looksFor, looksForLabels, onErrorHelp, refusalMessages, ruleInput, sourceLabels, stricterActions, toInput,
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

/**
 * Runs a guardrail write, with plain messages for its refusal reasons. A
 * loosening refusal names the platform rule whose tightening is no longer
 * stricter (`names`: rule ids to names).
 */
export async function guardrailWrite<T>(write: () => Promise<T>, names: Record<string, string> = {}): Promise<T> {
  try { return await write(); } catch (error) {
    if (error instanceof ApiError && error.reason === "guardrail_loosen_not_allowed" && error.detail?.rule) {
      const name = names[error.detail.rule];
      throw new ApiError(error.status, error.code, `This workspace's tightening of ${name ? `“${name}”` : "a platform rule"} is no longer stricter than the platform rule. Remove it, then try again.`, error.reason, error.detail);
    }
    if (error instanceof ApiError && error.reason && refusalMessages[error.reason]) throw new ApiError(error.status, error.code, refusalMessages[error.reason]!, error.reason, error.detail);
    throw error;
  }
}
/**
 * The layer a write was built from (`expected`): the server refuses the
 * write (409 `guardrail_layer_changed`) when the layer stored now is another
 * one, so a page loaded before another admin's change never undoes it.
 */
export const expectedOf = (set: RuleSet | null | undefined) => set ? { rule_set_id: set.id, revision: set.revision } : null;
/** A write that refetches its page's layer when the server finds it changed, so the next try is built from what is stored. */
export const refetchOnConflict = (refetch: () => unknown) => <T,>(write: Promise<T>): Promise<T> => write.catch((error: unknown) => {
  if (error instanceof ApiError && error.reason === "guardrail_layer_changed") void refetch();
  throw error;
});

const actionTone = (a?: InputAction) => a === "block" ? "danger" as const : a === "monitor" ? "info" as const : "neutral" as const;
export function ActionBadge({ action }: { action?: InputAction }) { return <Badge size="sm" tone={actionTone(action)}>{actionLabels[action ?? "off"]}</Badge>; }

/** The rules in effect, each with its provenance, status and sentence. */
export function EffectiveRules({ entries, caption, tighten }: { entries: EffectiveEntry[]; caption: string; /** Local layer writers: tighten a platform rule. */ tighten?: (e: EffectiveEntry) => void }) {
  const rules = entries;
  if (rules.length === 0) return <EmptyState size="compact" icon={<ShieldCheck />} title="No guardrail rules." description="Requests are not inspected." />;
  return <Table caption={caption} stack columns={["Rule", "Set by", "On input", "Status", ...(tighten ? [""] : [])]}>{rules.map(e => {
    const dangling = e.inactive_reason === "tightened_rule_missing";
    // An active rule whose input action is off checks nothing: it is shown as off, not active.
    const status = !e.active ? <Badge size="sm" tone="warning">{e.inactive_reason ? inactiveLabels[e.inactive_reason] : "Off"}</Badge> : e.input?.action === "off" ? <Badge size="sm">Off</Badge> : <Badge size="sm" tone="success">Active</Badge>;
    return <Tr key={e.id}>
      <Td><span className={s.primary}>{dangling ? "Tightening of a removed rule" : e.name}</span>{!dangling && <span className={s.secondary}>{inspectionSentence(e)}</span>}</Td>
      <Td><Badge size="sm" variant="outline">{sourceLabels[e.source]}</Badge>{e.tightened_by?.length ? <span className={s.secondary}>Tightened by this workspace</span> : null}</Td>
      <Td>{dangling ? "—" : <ActionBadge action={e.input?.action} />}</Td>
      <Td>{status}</Td>
      {tighten && <Td>{canTighten(e) && <Button size="sm" variant="ghost" aria-label={`Tighten ${e.name}`} onClick={() => tighten(e)}>Tighten</Button>}</Td>}
    </Tr>;
  })}</Table>;
}

/** A platform rule in effect that a local entry can still make stricter: in this release, a stricter input action. */
const canTighten = (e: EffectiveEntry) => e.source !== "workspace_local" && e.active && e.input?.action !== "block";

/** The server's `on_error` for a rule that names none: block and redact rules, and output stop or hold, fail closed. */
const defaultOnError = (rule: StoredRule) => rule.input.action === "block" || rule.input.action === "redact" || !["off", "monitor"].includes(rule.output?.mode ?? "off") ? "fail_closed" : "fail_open";
/** Custom patterns a layer allows in all (`write.rs` MAX_PATTERNS). */
const LAYER_PATTERNS = 50;
const patternCount = (e: Entry) => isTightening(e) ? 0 : e.detector.pattern_count ?? e.detector.patterns?.length ?? 0;

/** The rule dialog. `others`: the layer's patterns outside this rule, which share its limit. */
const ruleFields = (rule: StoredRule | undefined, others: number, actions: InputAction[] = ["monitor", "block"]): Field[] => {
  const looks = rule ? looksFor(rule) : "secrets", left = LAYER_PATTERNS - others;
  return [
    { name: "name", label: "Name", required: true, maxLength: 100, value: rule?.name ?? "" },
    { name: "protection", label: "Looks for", type: "select", required: true, value: looks, options: [
      { value: "secrets", label: "Secrets (keys, tokens, private keys, connection strings)" }, { value: "custom", label: "Custom patterns" },
      // Offered only to a rule that already has both (written through the API).
      ...(looks === "both" ? [{ value: "both", label: "Secrets and custom patterns" }] : []),
    ] },
    { name: "patterns", label: "Patterns", type: "textarea", value: rule?.detector.patterns?.join("\n") ?? "", visibleWhen: v => v.protection !== "secrets", requiredWhen: v => v.protection !== "secrets", help: `Regular expressions, one per line, 512 bytes each: at most ${LAYER_PATTERNS} in this layer, ${left} for this rule. Use ASCII classes like [0-9].`, validate: (v, all) => all.protection === "secrets" ? undefined : v.split("\n").filter(p => p.trim()).length > left ? `This layer allows ${LAYER_PATTERNS} patterns in all; ${left} are left for this rule.` : undefined },
    { name: "action", label: "On input", type: "select", required: true, value: rule?.input.action ?? "monitor", options: actions.map(a => ({ value: a, label: `${actionLabels[a]}: ${actionHelp[a]}` })) },
    // A stored value that is its action's default shows as the default, so changing the action changes it too.
    { name: "on_error", label: "If the rule can't run", type: "select", value: rule?.on_error && rule.on_error !== defaultOnError(rule) ? rule.on_error : "", help: onErrorHelp, options: [{ value: "", label: "Default: refused for Block, goes on for Monitor" }, { value: "fail_closed", label: "Refuse the request" }, { value: "fail_open", label: "Let the request go on and record it" }] },
    { name: "workloads", label: "Requests", type: "checkboxes", required: true, value: JSON.stringify(rule?.workloads ?? ["generation", "embeddings"]), options: [{ value: "generation", label: "Generation (chat, responses, messages)" }, { value: "embeddings", label: "Embeddings" }] },
  ];
};

/** How far a platform layer's change reaches, stated in its dialogs. */
export type Reach = { change: string; remove: string };

/**
 * One layer's own entries. `save` receives the layer's full entry list.
 * Platform layers also carry `local_only` (kept text inside the gateway).
 */
export function LayerEditor({ title, description, set, writable, save, localOnly, onLocalOnly, empty, marks, reach, names }: {
  title: string; description?: string; set: RuleSet | null; writable: boolean; save: (rules: Record<string, unknown>[], signal: AbortSignal) => Promise<unknown>;
  /** Platform layers: whether rules must keep text inside the gateway. */ localOnly?: boolean; onLocalOnly?: (value: boolean, signal: AbortSignal) => Promise<unknown>; empty?: string;
  /** Local layers: tightenings that no longer apply in full, with what the next save does to them. */ marks?: ReadonlyMap<string, string>;
  /** Platform layers: who an edit or removal affects. */ reach?: Reach;
  /** Platform rule ids to names, for a loosening refusal. */ names?: Record<string, string>;
}) {
  const ask = useAction(), entries = set?.rules ?? [], all = () => entries.map(toInput);
  const others = (id?: string) => entries.filter(e => e.id !== id).reduce((n, e) => n + patternCount(e), 0);
  const write = <T,>(run: () => Promise<T>) => guardrailWrite(run, names);
  const add = () => ask({ title: `Add a rule to ${title.toLowerCase()}`, description: reach?.change, fields: ruleFields(undefined, others()), submitLabel: "Add rule", successNotice: "Rule added.", run: (v, signal) => write(() => save([...all(), ruleInput(v)], signal)) });
  const edit = (rule: StoredRule) => ask({ title: `Edit ${rule.name}`, description: reach?.change, fields: ruleFields(rule, others(rule.id), rule.input.action === "off" ? ["off", "monitor", "block"] : ["monitor", "block"]), submitLabel: "Save rule", successNotice: "Rule saved.", run: (v, signal) => write(() => save(entries.map(e => e.id === rule.id ? ruleInput(v, toInput(rule)) : toInput(e)), signal)) });
  const remove = (e: Entry) => ask({ title: isTightening(e) ? "Remove this tightening?" : `Remove ${(e as StoredRule).name}?`, description: isTightening(e) ? "The platform rule runs as the platform set it." : `Requests are no longer checked by this rule.${reach ? ` ${reach.remove}` : ""}`, danger: true, submitLabel: "Remove", successNotice: "Rule removed.", run: (_, signal) => write(() => save(entries.filter(x => x.id !== e.id).map(toInput), signal)) });
  const local = () => ask({ title: "Keep text inside the gateway?", description: `When on, no rule of this layer or of a workspace's own rules may send text out of the gateway to be checked. Every rule in this release checks text inside the gateway.${reach ? ` ${reach.change}` : ""}`, fields: [{ name: "local_only", label: "Keep text inside the gateway", type: "switch", value: String(!!localOnly) }], submitLabel: "Save", run: (v, signal) => write(() => onLocalOnly!(v.local_only === "true", signal)) });
  return <Card title={title} titleAs="h2" description={description} flush={entries.length > 0}
    actions={writable && <Inline gap={2}>{onLocalOnly && <Button size="sm" variant="ghost" onClick={local}>Text stays inside: {localOnly ? "On" : "Off"}</Button>}<Button size="sm" variant="secondary" onClick={add}>Add rule</Button></Inline>}>
    {entries.length === 0 ? <EmptyState size="compact" icon={<ShieldCheck />} title={empty ?? "No rules."} description={writable ? "Add a rule to check requests." : undefined} /> :
      <Table caption={title} stack columns={["Rule", "On input", ...(writable ? [""] : [])]}>{entries.map(e => isTightening(e)
        ? <Tr key={e.id}><Td><span className={s.primary}>{marks?.has(e.id) ? "Tightening that no longer applies in full" : "Tightens a platform rule"}</span><span className={s.secondary}>{marks?.get(e.id) ?? (e.input?.action ? `On input: ${actionLabels[e.input.action]}` : "Refuses the request if the rule can't run")}</span></Td><Td><ActionBadge action={e.input?.action} /></Td>{writable && <Td><Button size="sm" variant="ghost" aria-label="Remove tightening" onClick={() => remove(e)}>Remove</Button></Td>}</Tr>
        : <Tr key={e.id}><Td><span className={s.primary}>{e.name}</span><span className={s.secondary}>{looksForLabels[looksFor(e)]} · {inspectionSentence(e)}</span></Td><Td><ActionBadge action={e.input.action} /></Td>{writable && <Td><Inline gap={1}><Button size="sm" variant="ghost" aria-label={`Edit ${e.name}`} onClick={() => edit(e)}>Edit</Button><Button size="sm" variant="ghost" aria-label={`Remove ${e.name}`} onClick={() => remove(e)}>Remove</Button></Inline></Td>}</Tr>)}
      </Table>}
  </Card>;
}

/**
 * "Tighten…": one platform rule made stricter by a tightening entry of the
 * local layer, with a stricter input action. `rule` is the rule as it runs
 * (already tightened), so the choices start from what is in effect; any
 * other setting of an existing tightening is kept (the save leaves out what
 * is no longer stricter than the live platform rule).
 */
export function tightenRule(ask: ReturnType<typeof useAction>, rule: EffectiveEntry, local: RuleSet | null, save: (rules: Record<string, unknown>[], signal: AbortSignal) => Promise<unknown>, names: Record<string, string> = {}) {
  const current = rule.input?.action ?? "off", options = stricterActions(current).filter(a => a !== current);
  const others = (local?.rules ?? []).filter(e => !(isTightening(e) && e.tightens === rule.id));
  const existing = (local?.rules ?? []).find((e): e is Tightening => isTightening(e) && e.tightens === rule.id);
  ask({ title: `Tighten ${rule.name}`, description: "This workspace can make a platform rule stricter, never weaker.", fields: [
    { name: "action", label: "On input", type: "select", required: true, value: options[options.length - 1] ?? "block", options: options.map(a => ({ value: a, label: `${actionLabels[a]}: ${actionHelp[a]}` })) },
  ],
    submitLabel: "Tighten", successNotice: "Rule tightened.",
    run: (v, signal) => {
      const { input: _input, ...kept } = existing ? toInput(existing) : {};
      return guardrailWrite(() => save([...others.map(toInput), { ...kept, tightens: rule.id, input: { action: v.action } }], signal), names);
    } });
}

/** PUT a platform layer (type default or override) built from `from`: `local_only` is stated every time. */
export const putPlatformLayer = (path: string, from: RuleSet | null | undefined, localOnly: boolean, rules: Record<string, unknown>[], signal: AbortSignal) => api(path, { method: "PUT", body: { local_only: localOnly, expected: expectedOf(from), rules }, signal });
