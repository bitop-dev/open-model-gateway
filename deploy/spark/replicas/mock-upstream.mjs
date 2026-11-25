// The model server of `spark-env.py up --mode replicas`: a tiny
// OpenAI-compatible chat endpoint inside the namespace that costs nothing.
// It answers every chat completion with a fixed reply and exact usage, and
// counts the calls it served (GET /__mock/calls). No prompt is stored.
import http from "node:http";
let calls = 0;
const server = http.createServer(async (req, res) => {
  const json = (status, body) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };
  if (req.method === "GET" && req.url === "/__mock/calls") return json(200, { calls });
  if (req.method === "POST" && req.url === "/v1/chat/completions") {
    let size = 0;
    let body = "";
    for await (const chunk of req) {
      size += chunk.length;
      if (size > 1 << 20) return json(413, { error: { message: "too large" } });
      body += chunk;
    }
    let model = "mock";
    try {
      model = JSON.parse(body).model ?? model;
    } catch {
      return json(400, { error: { message: "invalid json" } });
    }
    calls += 1;
    return json(200, {
      id: `chatcmpl-${calls}`, object: "chat.completion", created: Math.floor(Date.now() / 1000), model,
      choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 7, completion_tokens: 1, total_tokens: 8 },
    });
  }
  json(404, { error: { message: "not found" } });
});
server.listen(8000, "0.0.0.0");
process.on("SIGTERM", () => server.close(() => process.exit(0)));
