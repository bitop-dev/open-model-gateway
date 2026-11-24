// Official Anthropic SDK: token counting through the gateway (#72). The
// first key's model is a native Anthropic route (the provider counts); the
// second key's model is translated (the gateway estimates). The Rust caller
// checks what reached the upstream and that nothing durable was written.
import assert from "node:assert/strict";
import Anthropic from "@anthropic-ai/sdk";

const baseURL = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(baseURL).hostname, "127.0.0.1");
const native = new Anthropic({ baseURL, apiKey: process.env.SDK_TEST_API_KEY, maxRetries: 0 });
const translated = new Anthropic({ baseURL, apiKey: process.env.SDK_TEST_OTHER_KEY, maxRetries: 0 });
const ask = { system: "be brief", messages: [{ role: "user", content: "hello there" }],
  tools: [{ name: "lookup", input_schema: { type: "object" } }] };

const provider = await native.messages.countTokens({ model: "company/smart", ...ask }).withResponse();
assert.equal(provider.data.input_tokens, 42);
assert.equal(provider.response.headers.get("x-omg-token-count"), "provider");
// The beta namespace sends ?beta=true and the client's beta values.
const beta = await native.beta.messages.countTokens({ model: "company/smart", ...ask, betas: ["token-counting-2024-11-01"] });
assert.equal(beta.input_tokens, 42);

const estimate = await translated.messages.countTokens({ model: "company/team-only", ...ask }).withResponse();
assert.equal(estimate.data.input_tokens, 11);
assert.equal(estimate.response.headers.get("x-omg-token-count"), "estimate; method=utf8-bytes-div-4");

// Workspace and model come from the key: each key is refused the other's model.
await assert.rejects(native.messages.countTokens({ model: "company/team-only", ...ask }), (e) => e instanceof Anthropic.NotFoundError);
await assert.rejects(translated.messages.countTokens({ model: "company/smart", ...ask }), (e) => e instanceof Anthropic.NotFoundError);
await assert.rejects(new Anthropic({ baseURL, apiKey: "omg_not_a_key", maxRetries: 0 }).messages.countTokens({ model: "company/smart", ...ask }),
  (e) => e instanceof Anthropic.AuthenticationError);
