import { describe, expect, it } from "vitest";
import { markup } from "../lib/test-fixtures";
import { drainUntil, drainView, type Drain } from "../lib/governance";
import { ConnectionDrainPanel, RouteDrain, connectionDrainPath, deploymentDrainPath } from "./route-drain";
import { ActionProvider } from "./ui";

const drain = (over: Partial<Drain> = {}): Drain => ({ reason: "Replacing the GPU node", until: null, state: "active", drained_at: "2026-11-26T10:00:00Z", drained_by: { id: "u1", label: "Operator One" }, ...over });

describe("a drain in words (#49)", () => {
  it("states an active drain, its end or none, who and when", () => {
    expect(drainView(null)).toEqual({ state: "Not drained", facts: [] });
    const open = drainView(drain());
    expect(open.state).toBe("Drained · takes no new requests");
    expect(open.facts).toEqual([
      { label: "Until", value: "Until someone ends it" },
      { label: "Reason", value: "Replacing the GPU node" },
      { label: "Drained by", value: "Operator One" },
      { label: "Drained at", value: "2026-11-26T10:00:00Z", time: true },
    ]);
    expect(drainView(drain({ until: "2026-11-26T12:00:00Z" })).facts[0]).toEqual({ label: "Until", value: "2026-11-26T12:00:00Z", time: true });
  });
  it("reads an ended drain as ended, never as active", () => {
    const ended = drainView(drain({ until: "2026-11-26T11:00:00Z", state: "ended" }));
    expect(ended.state).toBe("Drain ended · takes requests again");
    expect(ended.facts[0]).toEqual({ label: "Ended at", value: "2026-11-26T11:00:00Z", time: true });
  });
  it("times a drain from the gateway's clock", () => {
    expect(drainUntil("2026-11-26T10:00:00Z", "")).toBeNull();
    expect(drainUntil("2026-11-26T10:00:00Z", "2")).toBe("2026-11-26T12:00:00.000Z");
  });
});

describe("drain sections", () => {
  it("shows a route drained through its connection, with its reason as text", () => {
    const path = deploymentDrainPath("d1");
    const html = markup(<ActionProvider><RouteDrain id="d1" name="Route" writable={false} /></ActionProvider>, [[path, { drain: null, connection: { id: "c1", name: "Primary <b>", drain: drain({ reason: "<script>x</script>" }) }, drained: true, as_of: "2026-11-26T10:00:41Z" }]]);
    expect(html).toContain("Not drained");
    expect(html).toContain("Drained through its connection Primary &lt;b&gt;");
    expect(html).toContain("&lt;script&gt;x&lt;/script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain("This route takes no new requests now.");
    expect(html).not.toContain("Drain route…");
  });
  it("offers an admin the drain's actions and counts the routes a connection drain takes out", () => {
    const path = connectionDrainPath("c1");
    const html = markup(<ActionProvider><ConnectionDrainPanel id="c1" name="Primary" writable /></ActionProvider>, [[path, { drain: drain({ until: "2026-11-26T09:00:00Z", state: "ended" }), routes: 3, as_of: "2026-11-26T10:00:41Z" }]]);
    expect(html).toContain("Drain ended · takes requests again");
    expect(html).toContain("Draining it would take out 3 enabled routes on this connection.");
    expect(html).toContain("Drain connection…");
    expect(html).toContain("Clear ended drain");
  });
});
