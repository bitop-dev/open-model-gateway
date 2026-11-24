// Official Anthropic SDK against a translated (OpenAI-style) Messages route:
// provider reasoning arrives as unsigned thinking blocks, complete and
// streamed; replaying them is accepted; structured output and strict tools
// are accepted. The Rust caller checks what reached the upstream.
import assert from "node:assert/strict";
import Anthropic from "@anthropic-ai/sdk";

const baseURL = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(baseURL).hostname, "127.0.0.1");
const client = new Anthropic({ baseURL, apiKey: process.env.SDK_TEST_API_KEY, maxRetries: 0 });
const ask = { model: "company/smart", max_tokens: 2048, messages: [{ role: "user", content: "hello" }],
  thinking: { type: "enabled", budget_tokens: 1024 }, output_config: { effort: "high" } };
const expected = [{ type: "thinking", thinking: "plan" }, { type: "text", text: "answer" }];

const complete = await client.messages.create(ask);
assert.deepEqual(complete.content, expected);
assert.equal(complete.stop_reason, "end_turn");

const stream = client.messages.stream(ask);
const starts = [];
stream.on("streamEvent", (event) => {
  if (event.type === "content_block_start") starts.push([event.index, event.content_block.type]);
});
const streamed = await stream.finalMessage();
assert.deepEqual(streamed.content, expected);
assert.deepEqual(starts, [[0, "thinking"], [1, "text"]]);
assert.equal(await stream.finalText(), "answer");

// Claude Code replays the unsigned blocks; the translated route removes them.
const replay = await client.messages.create({ ...ask, messages: [...ask.messages,
  { role: "assistant", content: streamed.content }, { role: "user", content: "continue" }] });
assert.equal(replay.type, "message");

const schema = { type: "object", properties: { answer: { type: "string" } }, required: ["answer"], additionalProperties: false };
const structured = await client.messages.create({ model: "company/smart", max_tokens: 64,
  messages: [{ role: "user", content: "hello" }], output_config: { format: { type: "json_schema", schema } },
  tools: [{ name: "lookup", input_schema: { type: "object" }, strict: true }] });
assert.equal(structured.type, "message");
