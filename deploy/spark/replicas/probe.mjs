// One connection from this pod to the gateway Service for step 6 of
// `spark-env.py check` (the chart's NetworkPolicy): prints {"probe": <HTTP
// status>} when the gateway answered, or "timeout" / "refused" / another
// error kind when the connection did not get through. No key, no body.
let probe;
try {
  const response = await fetch("http://omg:8080/health/live", { signal: AbortSignal.timeout(8000) });
  await response.arrayBuffer();
  probe = response.status;
} catch (error) {
  const kind = error?.cause?.code ?? error?.name ?? "error";
  probe = kind === "TimeoutError" || kind === "UND_ERR_CONNECT_TIMEOUT" ? "timeout" : kind === "ECONNREFUSED" ? "refused" : kind;
}
console.log(JSON.stringify({ probe }));
