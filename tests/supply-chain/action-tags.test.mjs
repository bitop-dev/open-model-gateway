import assert from "node:assert/strict";
import { test } from "node:test";
import { verifyActionTags } from "../../scripts/check-action-tags.mjs";

const pin = "1".repeat(40);
const other = "2".repeat(40);
const files = [{ file: "fixture.yml", text: `uses: github/codeql-action/analyze@${pin} # v4.0.0` }];
const reference = sha => ({ ref: "refs/tags/v4.0.0", object: { type: "commit", sha } });

test("refuses a pin that differs from the declared upstream tag's commit", async () => {
  await assert.rejects(verifyActionTags(files, async () => reference(other)), /fixture.yml.*v4.0.0.*mismatch/);
});

const refPath = "/repos/github/codeql-action/git/ref/tags/v4.0.0";
const tagPath = `/repos/github/codeql-action/git/tags/${other}`;

for (const annotated of [false, true]) {
  test(`resolves a ${annotated ? "annotated" : "lightweight"} tag once for multiple sub-actions`, async () => {
    const calls = [];
    const get = async path => {
      calls.push(path);
      if (path === tagPath) return { sha: other, object: { type: "commit", sha: pin } };
      return annotated
        ? { ref: "refs/tags/v4.0.0", object: { type: "tag", sha: other } }
        : reference(pin);
    };
    const init = { ...files[0], text: files[0].text.replace("/analyze", "/init") };
    assert.equal(await verifyActionTags([...files, init], get), 1);
    assert.deepEqual(calls, annotated ? [refPath, tagPath] : [refPath]);
  });
}

test("checks each pin even when its upstream result is cached", async () => {
  const conflict = { ...files[0], text: files[0].text.replace(pin, other) };
  await assert.rejects(verifyActionTags([...files, conflict], async () => reference(pin)), /mismatch/);
});

test("refuses a missing tag, a different ref, or a tag pointing to a tree", async () => {
  await assert.rejects(verifyActionTags(files, async () => { throw new Error("HTTP 404"); }), /HTTP 404/);
  await assert.rejects(verifyActionTags(files, async () => ({ ...reference(pin), ref: "refs/heads/v4.0.0" })), /unexpected tag reference/);
  await assert.rejects(verifyActionTags(files, async () => ({ ...reference(pin), object: { type: "tree", sha: pin } })), /did not resolve to a commit/);
});

test("refuses a pin to the annotated tag object itself", async () => {
  const get = async path => path === refPath
    ? { ref: "refs/tags/v4.0.0", object: { type: "tag", sha: pin } }
    : { sha: pin, object: { type: "commit", sha: other } };
  await assert.rejects(verifyActionTags(files, get), /mismatch/);
});
