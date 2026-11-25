// The connections of a step 6 probe pod of `spark-env.py check` (the chart's
// NetworkPolicy): first a control to the free model server, which the chart's
// policy does not cover (it shows this pod's network works), then the gateway
// Service. Prints {"control": ..., "probe": ...}: the HTTP status when the
// server answered, or "timeout" / "refused" / another error kind when the
// connection did not get through. No key, no body.
async function reach(url) {
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(8000) });
    await response.arrayBuffer();
    return response.status;
  } catch (error) {
    const kind = error?.cause?.code ?? error?.name ?? "error";
    if (kind === "TimeoutError" || kind === "UND_ERR_CONNECT_TIMEOUT") return "timeout";
    return kind === "ECONNREFUSED" ? "refused" : String(kind);
  }
}
const control = await reach("http://omg-mock-upstream:8000/__mock/calls");
const probe = await reach("http://omg:8080/health/live");
console.log(JSON.stringify({ control, probe }));
