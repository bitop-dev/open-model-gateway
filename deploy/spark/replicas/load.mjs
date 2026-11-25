// Steady request load for `spark-env.py check` (the rolling restart step),
// from inside the namespace to the gateway Service, so the Service's own
// endpoint changes are what is measured (a port-forward sticks to one pod).
// Sends chat completions with the key in $OMG_KEY, $CONCURRENCY at a time,
// until the check creates /tmp/stop (after the rollout) or $SECONDS_TOTAL
// pass, and prints one JSON line of counts by status and failure kind:
// never the key, a body or a header.
const base = process.env.OMG_BASE;
const key = process.env.OMG_KEY;
const model = process.env.OMG_MODEL;
const seconds = Number(process.env.SECONDS_TOTAL ?? "60");
const concurrency = Number(process.env.CONCURRENCY ?? "4");
import { existsSync } from "node:fs";
const until = Date.now() + seconds * 1000;
const running = () => Date.now() < until && !existsSync("/tmp/stop");
const counts = { sent: 0, statuses: {}, failures: {} };
async function worker() {
  while (running()) {
    counts.sent += 1;
    try {
      const response = await fetch(`${base}/v1/chat/completions`, {
        method: "POST",
        headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
        body: JSON.stringify({ model, messages: [{ role: "user", content: "hello" }], max_tokens: 4 }),
        signal: AbortSignal.timeout(20000),
      });
      await response.arrayBuffer();
      counts.statuses[response.status] = (counts.statuses[response.status] ?? 0) + 1;
    } catch (error) {
      const kind = error?.cause?.code ?? error?.name ?? "error";
      counts.failures[kind] = (counts.failures[kind] ?? 0) + 1;
    }
  }
}
await Promise.all(Array.from({ length: concurrency }, worker));
console.log(JSON.stringify(counts));
