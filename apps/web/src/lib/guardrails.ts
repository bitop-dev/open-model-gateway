/*
 * Guardrails (#840, docs/guardrails-design.md §5): the rule layers of a
 * workspace and their management API. Phase 1 runs the pattern detector
 * inside the gateway (built-in secret shapes and custom patterns), on the
 * input of generation and embeddings requests, with monitor and block.
 * Patterns are configuration, shown only to the layer's readers of its
 * configuration (others see a count); matched text is never shown or sent.
 */
import { API, platformWorkspacePath, wsPath } from "./api";
import { parseCheckboxValues } from "./forms";

export type WorkspaceKind = "personal" | "team" | "project";
export type Layer = "type_default" | "workspace_override" | "workspace_local";
export type InputAction = "off" | "monitor" | "redact" | "block";
export type Protection = "secrets" | "personal_data" | "moderation" | "output_dlp" | "prompt_injection" | "custom";
export type Category = "secret" | "personal_data" | "moderation" | "prompt_injection" | "custom";
export type Workload = "generation" | "embeddings" | "rerank" | "systemone" | "audio_speech" | "images" | "audio_transcriptions" | "realtime";
export type InactiveReason = "egress_not_allowed" | "tightened_rule_missing" | "not_runnable";
export type Egress = "local" | "model" | "service";

export type Detector = { kind: "pattern" | "systemone" | "llm_judge" | "external"; categories: Category[]; patterns?: string[]; pattern_count?: number; egress?: Egress };
/** A stored rule of a layer. */
export type StoredRule = {
  id: string; name: string; protection: Protection; detector: Detector;
  input: { action: InputAction; redact_scope?: string }; output?: { mode: string }; on_error?: "fail_open" | "fail_closed";
  workloads?: Workload[]; uninspectable?: "allow" | "deny"; threshold?: number | null; timeout_ms?: number | null;
};
/** A local layer's entry that makes a platform rule stricter. */
export type Tightening = { id: string; tightens: string; input?: { action?: InputAction }; output?: { mode: string }; on_error?: "fail_open" | "fail_closed"; threshold?: number | null };
export type Entry = StoredRule | Tightening;
export type RuleSet = { id: string; layer: Layer; revision: number; local_only: boolean | null; rules: Entry[] };
/** An effective entry: a rule as it runs (tightened), with its provenance. */
export type EffectiveEntry = Partial<StoredRule> & {
  id: string; rule_set_id: string; revision: number; source: Layer; active: boolean; inactive_reason: InactiveReason | null;
  tightened_by?: string[]; tightens?: string;
};
export type WorkspaceGuardrails = {
  workspace_id: string; kind: WorkspaceKind; enforced: boolean; inspected_workloads: Workload[]; can_edit: boolean;
  platform: { source: "type_default" | "workspace_override"; rule_set: RuleSet | null }; egress: { local_only: boolean };
  local: RuleSet | null; effective: EffectiveEntry[];
};
export type DecisionTotal = { action: string; category: Category; decisions: number; findings: number };
export type PlatformWorkspaceGuardrails = Omit<WorkspaceGuardrails, "can_edit" | "effective"> & {
  type_default: RuleSet | null; override: RuleSet | null; local_visible: boolean; effective: EffectiveEntry[] | null; decisions_this_month: DecisionTotal[];
};
export type TypeDefault = { kind: WorkspaceKind; rule_set: RuleSet | null; inspected_workloads: Workload[] };
export type DetectorInfo = { kind: Detector["kind"]; egress: Egress; available: boolean; categories: Category[]; input_actions: InputAction[]; output_modes: string[]; workloads: Workload[] };
export type Detectors = { detectors: DetectorInfo[]; inspected_workloads: Workload[] };

export const guardrailsPath = (ws: string) => `${wsPath(ws)}/guardrails`;
export const platformGuardrailsPath = (ws: string) => `${platformWorkspacePath(ws)}/guardrails`;
export const typeDefaultPath = (kind: WorkspaceKind) => `${API}/platform/workspace-types/${kind}/guardrails`;
export const detectorsPath = `${API}/platform/guardrails/detectors`;

export const isTightening = (e: Entry | EffectiveEntry): e is Tightening => "tightens" in e && !!e.tightens && !("protection" in e && e.protection);

export const actionLabels: Record<InputAction, string> = { off: "Off", monitor: "Monitor", redact: "Redact", block: "Block" };
export const actionHelp: Record<InputAction, string> = { off: "Not checked on input.", monitor: "Recorded; the request goes on.", redact: "Matches are replaced before sending.", block: "The request is refused before anything is sent." };
export const protectionLabels: Record<Protection, string> = { secrets: "Secrets", personal_data: "Personal data", moderation: "Moderation", output_dlp: "Output DLP", prompt_injection: "Prompt injection", custom: "Custom patterns" };
export const sourceLabels: Record<Layer, string> = { type_default: "Workspace-type default", workspace_override: "Platform override", workspace_local: "This workspace" };
export const workloadLabels: Partial<Record<Workload, string>> = { generation: "Generation", embeddings: "Embeddings" };
export const inactiveLabels: Record<InactiveReason, string> = { egress_not_allowed: "Off: would send text out of the gateway", tightened_rule_missing: "Off: its platform rule was removed", not_runnable: "Needs a later release" };
/** Strictness of input actions, for the tighten-only choices. */
const strictness: InputAction[] = ["off", "monitor", "redact", "block"];
export const stricterActions = (current: InputAction, offered: InputAction[] = ["off", "monitor", "block"]) => offered.filter(a => strictness.indexOf(a) >= strictness.indexOf(current));

const list = (items: string[]) => items.length <= 1 ? items.join("") : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
/**
 * What a rule inspects and where it goes, in one sentence (design §5,
 * "Dashboard"). The pattern detector runs inside the gateway: nothing is sent
 * anywhere else.
 */
export function inspectionSentence(e: Partial<StoredRule>): string {
  if (e.input?.action === "off") return "Off: checks nothing on input.";
  const count = e.detector?.pattern_count ?? e.detector?.patterns?.length ?? 0, looks = looksFor(e);
  const secrets = "secrets (keys, tokens, private keys, connection strings)", custom = `matches of ${count === 1 ? "1 custom pattern" : `${count} custom patterns`}`;
  const what = looks === "both" ? `${secrets} and ${custom}` : looks === "custom" ? custom : secrets;
  const where = list((e.workloads ?? ["generation", "embeddings"]).map(w => workloadLabels[w]?.toLowerCase() ?? w.replace(/_/g, " ")));
  const how = e.input?.action === "block" ? "and refuses a request that has one" : e.input?.action === "monitor" ? "and records what it finds; the request goes on" : "";
  const unreadable = e.input?.action === "block" ? " It also refuses a request it can't read (one with an image, audio or a document), and a native batch when it is created." : "";
  const egress = e.detector?.egress && e.detector.egress !== "local" ? "Text is sent out of the gateway to be checked." : "Checked inside the gateway; nothing is sent anywhere else.";
  // `on_error` covers only a rule that can't run at all (`onErrorHelp`).
  const failure = e.on_error === "fail_closed" ? " If the rule can't run, the request is refused." : e.on_error === "fail_open" ? " If the rule can't run, the request goes on and that is recorded." : "";
  return `Looks for ${what} in the input of ${where} requests${how ? ` ${how}` : ""}.${unreadable}${failure} ${egress}`;
}
export const inspectedSentence = (workloads: Workload[]) => `Guardrails check the input of ${list(workloads.map(w => workloadLabels[w]?.toLowerCase() ?? w))} requests (also token counts and gateway-run batch lines). Other workloads are not checked yet, and response text is never checked in this release. A Block rule also refuses a request it can't read (one with an image, audio or a document) and a native batch when it is created. When the gateway is too busy to check a request, it refuses it with a retryable error (503) under any rule, Monitor included: nothing goes on unchecked.`;
/**
 * What a rule's `on_error` covers (enforce.rs `failed`, design §4): only a
 * rule that can't run at all. A busy gateway or a spent check budget refuses
 * the request with a retryable 503 whatever `on_error` says.
 */
export const onErrorHelp = "Applies only when the rule can't run at all: a stored pattern that no longer compiles, or a rule that needs a later release. A busy gateway never lets a request through unchecked: under any rule, Monitor included, it refuses the request with a retryable error (503).";
/** What a rule looks for, from its detector's categories (a rule written through the API may have both). */
export type LooksFor = "secrets" | "custom" | "both";
export function looksFor(e: Partial<StoredRule>): LooksFor {
  const c = e.detector?.categories ?? [], secret = c.includes("secret"), custom = c.includes("custom");
  return secret && custom ? "both" : custom ? "custom" : secret ? "secrets" : e.protection === "custom" ? "custom" : "secrets";
}
export const looksForLabels: Record<LooksFor, string> = { secrets: "Secrets", custom: "Custom patterns", both: "Secrets and custom patterns" };

const present = <T,>(key: string, value: T | null | undefined) => value === null || value === undefined ? {} : { [key]: value };
/**
 * The PUT body entry of a stored entry. The layer is replaced whole, so every
 * entry is sent again with every setting the server returned: a field left
 * out would take its default (an `on_error` its action's), not its stored
 * value.
 */
export function toInput(e: Entry): Record<string, unknown> {
  if (isTightening(e)) return { id: e.id, tightens: e.tightens, ...(e.input?.action ? { input: { action: e.input.action } } : {}), ...present("output", e.output), ...present("on_error", e.on_error), ...present("threshold", e.threshold) };
  const rule = e as StoredRule;
  return {
    id: rule.id, name: rule.name, protection: rule.protection,
    detector: { kind: rule.detector.kind, categories: rule.detector.categories, ...(rule.detector.patterns?.length ? { patterns: rule.detector.patterns } : {}) },
    input: { action: rule.input.action, ...present("redact_scope", rule.input.redact_scope) }, ...present("output", rule.output), ...present("on_error", rule.on_error), ...present("workloads", rule.workloads),
    ...present("uninspectable", rule.uninspectable), ...present("threshold", rule.threshold), ...present("timeout_ms", rule.timeout_ms),
  };
}
/**
 * A rule's PUT entry from the rule dialog's values. Editing starts from the
 * stored rule (`base`, from `toInput`), so the settings the dialog does not
 * show keep their stored values.
 */
export function ruleInput(v: Record<string, string | undefined>, base?: Record<string, unknown>): Record<string, unknown> {
  const looks = (v.protection ?? "secrets") as LooksFor, patterns = (v.patterns ?? "").split("\n").map(p => p.trim()).filter(Boolean);
  const workloads = v.workloads ? parseCheckboxValues(v.workloads) : [];
  const { on_error: _stored, ...kept } = base ?? {};
  // The stored detector and protection are kept unless what the rule looks
  // for, or its patterns, changed: the server stores protection and
  // categories independently, and a rebuilt detector could drop either.
  const stored = base?.detector as { categories?: Category[]; patterns?: string[] } | undefined;
  const same = !!stored && looksFor({ detector: stored as Detector, protection: base?.protection as Protection }) === looks
    && (looks === "secrets" || JSON.stringify(stored.patterns ?? []) === JSON.stringify(patterns));
  const detector = same ? stored : { kind: "pattern", categories: looks === "both" ? ["secret", "custom"] : [looks === "custom" ? "custom" : "secret"], ...(looks !== "secrets" ? { patterns } : {}) };
  const protection = same ? base!.protection : looks === "secrets" ? "secrets" : looks === "custom" ? "custom" : (base?.protection ?? "custom");
  return {
    ...kept, name: (v.name ?? "").trim(), protection, detector,
    input: { ...(base?.input as object | undefined), action: v.action }, ...(v.on_error ? { on_error: v.on_error } : {}), ...(workloads.length ? { workloads } : {}),
  };
}

const order = <T extends string>(values: readonly T[]) => (v: T | undefined) => v === undefined ? -1 : values.indexOf(v);
const actionRank = order(strictness), outputRank = order(["off", "monitor", "stop", "block"] as const), errorRank = order(["fail_open", "fail_closed"] as const);
/**
 * A tightening entry (PUT shape) against its live platform rule: only the
 * settings still stricter than the rule's (the server refuses a weaker one,
 * `write.rs` `loosens`), or `null` when nothing stricter is left.
 */
export function stricterPart(t: Record<string, unknown>, base: StoredRule): Record<string, unknown> | null {
  const input = (t.input as { action?: InputAction } | undefined)?.action, output = (t.output as { mode?: string } | undefined)?.mode as "off" | "monitor" | "stop" | "block" | undefined;
  const onError = t.on_error as "fail_open" | "fail_closed" | undefined, threshold = t.threshold as number | undefined;
  const keep = {
    ...(input !== undefined && actionRank(input) > actionRank(base.input.action) ? { input: { action: input } } : {}),
    ...(output !== undefined && outputRank(output) > outputRank((base.output?.mode ?? "off") as "off") ? { output: { mode: output } } : {}),
    ...(onError !== undefined && base.on_error !== undefined && errorRank(onError) > errorRank(base.on_error) ? { on_error: onError } : {}),
    ...(threshold !== undefined && base.threshold !== null && base.threshold !== undefined && threshold < base.threshold ? { threshold } : {}),
  };
  return Object.keys(keep).length ? { ...(t.id ? { id: t.id } : {}), tightens: t.tightens, ...keep } : null;
}
/** Plain messages for the reasons a guardrail write is refused. */
export const refusalMessages: Record<string, string> = {
  guardrail_loosen_not_allowed: "A local rule can only make a platform rule stricter, never weaker.",
  guardrail_egress_not_allowed: "This workspace's platform rules keep text inside the gateway, so this rule isn't allowed.",
  guardrail_detector_unavailable: "That option isn't available in this release.",
  guardrail_pattern_invalid: "A pattern doesn't compile within the limits. Check it and try again.",
};
