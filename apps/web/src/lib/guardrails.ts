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
  const count = e.detector?.pattern_count ?? e.detector?.patterns?.length ?? 0;
  const what = e.protection === "custom" ? `matches of ${count === 1 ? "1 custom pattern" : `${count} custom patterns`}` : "secrets (keys, tokens, private keys, connection strings)";
  const where = list((e.workloads ?? ["generation", "embeddings"]).map(w => workloadLabels[w]?.toLowerCase() ?? w.replace(/_/g, " ")));
  const how = e.input?.action === "block" ? "and refuses a request that has one" : e.input?.action === "monitor" ? "and records what it finds; the request goes on" : "";
  const egress = e.detector?.egress && e.detector.egress !== "local" ? "Text is sent out of the gateway to be checked." : "Checked inside the gateway; nothing is sent anywhere else.";
  return `Looks for ${what} in the input of ${where} requests${how ? ` ${how}` : ""}. ${egress}`;
}
export const inspectedSentence = (workloads: Workload[]) => `Guardrails check the input of ${list(workloads.map(w => workloadLabels[w]?.toLowerCase() ?? w))} requests (also token counts and gateway-run batch lines). Other workloads are not checked yet, and response text is never checked in this release.`;

/** The PUT body entry of a stored entry: the layer is replaced whole, so every entry is sent again. */
export function toInput(e: Entry): Record<string, unknown> {
  if (isTightening(e)) return { id: e.id, tightens: e.tightens, ...(e.input?.action ? { input: { action: e.input.action } } : {}), ...(e.on_error ? { on_error: e.on_error } : {}) };
  const rule = e as StoredRule;
  return {
    id: rule.id, name: rule.name, protection: rule.protection,
    detector: { kind: rule.detector.kind, categories: rule.detector.categories, ...(rule.detector.patterns?.length ? { patterns: rule.detector.patterns } : {}) },
    input: { action: rule.input.action }, ...(rule.on_error ? { on_error: rule.on_error } : {}), ...(rule.workloads ? { workloads: rule.workloads } : {}),
  };
}
/** A new rule's PUT entry from the rule dialog's values. */
export function ruleInput(v: Record<string, string | undefined>, id?: string): Record<string, unknown> {
  const custom = v.protection === "custom", patterns = (v.patterns ?? "").split("\n").map(p => p.trim()).filter(Boolean);
  const workloads = v.workloads ? parseCheckboxValues(v.workloads) : [];
  return {
    ...(id ? { id } : {}), name: (v.name ?? "").trim(), protection: custom ? "custom" : "secrets",
    detector: { kind: "pattern", categories: [custom ? "custom" : "secret"], ...(custom ? { patterns } : {}) },
    input: { action: v.action }, ...(workloads.length ? { workloads } : {}),
  };
}
/** Plain messages for the reasons a guardrail write is refused. */
export const refusalMessages: Record<string, string> = {
  guardrail_loosen_not_allowed: "A local rule can only make a platform rule stricter, never weaker.",
  guardrail_egress_not_allowed: "This workspace's platform rules keep text inside the gateway, so this rule isn't allowed.",
  guardrail_detector_unavailable: "That option isn't available in this release.",
  guardrail_pattern_invalid: "A pattern doesn't compile within the limits. Check it and try again.",
};
