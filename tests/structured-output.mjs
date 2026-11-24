// Official Anthropic SDK: structured output and a strict tool through the
// gateway, complete and streamed. With "refused", the upstream rejects the
// schema and the client gets Anthropic's 400 invalid_request_error. The Rust
// caller checks what reached the upstream.
import assert from "node:assert/strict";
import Anthropic from "@anthropic-ai/sdk";

const baseURL = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(baseURL).hostname, "127.0.0.1");
const client = new Anthropic({ baseURL, apiKey: process.env.SDK_TEST_API_KEY, maxRetries: 0 });
const schema = { type: "object", properties: { answer: { type: "string" } }, required: ["answer"], additionalProperties: false };
const ask = { model: "company/smart", max_tokens: 32, messages: [{ role: "user", content: "hello" }],
  output_config: { format: { type: "json_schema", schema } },
  tools: [{ name: "lookup", input_schema: schema, strict: true }] };

if (process.argv[2] === "refused") {
  for (const send of [() => client.messages.create(ask), () => client.messages.stream(ask).finalMessage()]) {
    await assert.rejects(send(), (error) => error instanceof Anthropic.BadRequestError
      && error.error?.error?.type === "invalid_request_error"
      && !JSON.stringify(error.error).includes("SECRET"));
  }
} else {
  const complete = await client.messages.create(ask);
  assert.equal(complete.stop_reason, "end_turn");
  assert.deepEqual(JSON.parse(complete.content.find((b) => b.type === "text").text), { answer: "yes" });
  const streamed = await client.messages.stream(ask).finalMessage();
  assert.deepEqual(JSON.parse(streamed.content.find((b) => b.type === "text").text), { answer: "yes" });
}
