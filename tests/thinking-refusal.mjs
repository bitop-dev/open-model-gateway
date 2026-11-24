// Official Anthropic SDK through the gateway. "refusal": a translated model
// declines structured output; the client gets stop_reason "refusal" and no
// content, complete and streamed. "signature": an unsigned thinking block
// replayed with an empty signature on a native route is refused with 400
// invalid_request_error before admission. The Rust caller checks the rest.
import assert from "node:assert/strict";
import Anthropic from "@anthropic-ai/sdk";

const baseURL = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(baseURL).hostname, "127.0.0.1");
const client = new Anthropic({ baseURL, apiKey: process.env.SDK_TEST_API_KEY, maxRetries: 0 });
const schema = { type: "object", properties: { answer: { type: "string" } }, required: ["answer"], additionalProperties: false };

if (process.argv[2] === "refusal") {
  const ask = { model: "company/smart", max_tokens: 32, messages: [{ role: "user", content: "hello" }],
    output_config: { format: { type: "json_schema", schema } } };
  for (const reply of [await client.messages.create(ask), await client.messages.stream(ask).finalMessage()]) {
    assert.equal(reply.stop_reason, "refusal");
    assert.deepEqual(reply.content, []);
    assert.ok(!JSON.stringify(reply).includes("SECRET-REFUSAL"), "the refusal text is never returned");
  }
} else {
  for (const signature of ["", " "]) {
    const replay = { model: "company/smart", max_tokens: 32, messages: [
      { role: "user", content: "hello" },
      { role: "assistant", content: [{ type: "thinking", thinking: "plan", signature }, { type: "text", text: "answer" }] },
      { role: "user", content: "continue" }] };
    await assert.rejects(client.messages.create(replay), (e) => e instanceof Anthropic.BadRequestError
      && e.error?.error?.type === "invalid_request_error" && e.message.includes("thinking"));
  }
}
