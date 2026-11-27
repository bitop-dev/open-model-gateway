import { api } from "../lib/api";
import { detailTime } from "../lib/format";
import { DRAIN_REASON_MAX, drainEndDefault, drainRequestUntil, drainView, type ConnectionDrain, type DeploymentDrain, type Drain } from "../lib/governance";
import { Button, ErrorNotice, useAction, useApi } from "./ui";
import { DescriptionList } from "./ui/description-list/description-list";
import { Inline } from "./ui/layout/layout";

const platformPath = "/api/v1/platform";
export const deploymentDrainPath = (id: string) => `${platformPath}/deployments/${encodeURIComponent(id)}/drain`;
export const connectionDrainPath = (id: string) => `${platformPath}/providers/${encodeURIComponent(id)}/drain`;

function DrainFacts({ drain, prefix = "" }: { drain: Drain | null; prefix?: string }) {
  const view = drainView(drain);
  return <DescriptionList dividers items={[{ label: `${prefix}Drain`, value: view.state }, ...view.facts.map(f => ({ label: f.label, value: f.time ? detailTime(f.value) : f.value }))]} />;
}

/** Set or change a drain: a reason, and how many hours it lasts (from the gateway's clock), or until someone ends it. */
function DrainActions({ path, name, drain, asOf, noun }: { path: string; name: string; drain: Drain | null; asOf: string; noun: string }) {
  const ask = useAction(), active = drain?.state === "active";
  const edit = () => ask({
    title: `${active ? "Change drain" : "Drain"} · ${name}`,
    description: `A drained ${noun} takes no new requests; requests already running finish. It stays enabled, listed and selectable, and returns on its own when the drain ends.`,
    submitLabel: active ? "Save drain" : `Drain ${noun}`,
    successNotice: active ? "Drain changed." : `${noun[0].toUpperCase()}${noun.slice(1)} drained.`,
    fields: [
      { name: "reason", label: "Reason", type: "textarea", required: true, maxLength: DRAIN_REASON_MAX, value: active ? drain.reason : "", help: "Shown to admins and auditors and kept in the audit log; never shown to API clients." },
      { name: "end", label: "Ends", type: "select", required: true, value: drainEndDefault(drain), options: [...(active && drain.until ? [{ value: "keep", label: `Keep the current end (${detailTime(drain.until)})` }] : []), { value: "hours", label: "After a number of hours" }, { value: "none", label: "No end: until someone ends it" }] },
      { name: "hours", label: "Ends after (hours)", type: "number", min: 1, max: 8784, value: "", requiredWhen: values => values.end === "hours", visibleWhen: values => values.end === "hours", help: "Counted from when you save." },
    ],
    // `until` is measured from the gateway's clock at submit, not at page load.
    run: async (values, signal) => {
      const now = values.end === "hours" ? (await api<{ as_of: string }>(path, { signal })).as_of : asOf;
      return api(path, { method: "PUT", body: { reason: values.reason, until: drainRequestUntil(values.end ?? "none", values.hours ?? "", drain, now) }, signal });
    },
  });
  const end = () => ask({ title: `End drain · ${name}`, description: `The ${noun} takes new requests again at once.`, submitLabel: "End drain", successNotice: "Drain ended.", run: (_, signal) => api(path, { method: "DELETE", signal }) });
  return <Inline gap={2}><Button size="sm" variant="secondary" onClick={edit}>{active ? "Change drain…" : `Drain ${noun}…`}</Button>{drain && <Button size="sm" variant="secondary" onClick={end}>{active ? "End drain" : "Clear ended drain"}</Button>}</Inline>;
}

/** A route's drain and its connection's, beside its health (#49). */
export function RouteDrain({ id, name, writable }: { id: string; name: string; writable: boolean }) {
  const q = useApi<DeploymentDrain>(deploymentDrainPath(id));
  if (q.isPending) return <p role="status">Loading drain…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const { drain, connection } = q.data, viaConnection = connection.drain?.state === "active";
  return <>
    <DrainFacts drain={drain} />
    {viaConnection && <><p>Drained through its connection {connection.name}: no route on it takes new requests.</p><DrainFacts drain={connection.drain} prefix="Connection " /></>}
    <p>{q.data.drained ? "This route takes no new requests now." : "This route takes requests (subject to its health above)."} As of {detailTime(q.data.as_of)}.</p>
    {writable && <DrainActions path={deploymentDrainPath(id)} name={name} drain={drain} asOf={q.data.as_of} noun="route" />}
  </>;
}

/** A connection's drain and the routes it takes out while active (#49). */
export function ConnectionDrainPanel({ id, name, writable }: { id: string; name: string; writable: boolean }) {
  const q = useApi<ConnectionDrain>(connectionDrainPath(id));
  if (q.isPending) return <p role="status">Loading drain…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const { drain, routes } = q.data, count = `${routes} enabled route${routes === 1 ? "" : "s"}`;
  return <>
    <DrainFacts drain={drain} />
    <p>{drain?.state === "active" ? `Takes out ${count} on this connection.` : `Draining it would take out ${count} on this connection.`} As of {detailTime(q.data.as_of)}.</p>
    {writable && <DrainActions path={connectionDrainPath(id)} name={name} drain={drain} asOf={q.data.as_of} noun="connection" />}
  </>;
}
