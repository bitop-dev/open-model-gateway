import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { README, readFromReadme, renumber, resolveCitations } from "../scripts/formal-citations.mjs";

const cite = (line, token = "FOR UPDATE OF r") => `[\`${token}\`](../../apps/gateway/src/governance/settlement.rs#L${line})`;
const lines = citations => citations.map(citation => citation.line);

test("the cited line wins while it holds the text, even when the text occurs elsewhere", () => {
  const source = "FOR UPDATE OF r\nSELECT ... FOR UPDATE OF r\nFOR UPDATE OF r";
  assert.deepEqual(lines(resolveCitations(cite(2), () => source)), [2]);
});

test("a citation whose line moved follows text that still identifies one line", () => {
  assert.deepEqual(lines(resolveCitations(cite(2), () => "\n\n\nSELECT ... FOR UPDATE OF r")), [4]);
  // Every occurrence is cited: the moved ones keep their order.
  const two = `${cite(1)} and ${cite(2)}`;
  assert.deepEqual(lines(resolveCitations(two, () => "\n\nFOR UPDATE OF r\n\nFOR UPDATE OF r")), [3, 5]);
  // One citation still exact, the other moved: the moved one takes the uncited occurrence.
  assert.deepEqual(lines(resolveCitations(`${cite(1)} and ${cite(4)}`, () => "FOR UPDATE OF r\n\nFOR UPDATE OF r")), [1, 3]);
});

test("text that is gone or no longer identifies one line fails", () => {
  assert.throws(() => resolveCitations(cite(2), () => "SELECT 1\nSELECT 2"), /no longer contains FOR UPDATE OF r/);
  // Moved, and two candidate lines for one citation.
  assert.throws(
    () => resolveCitations(cite(2), () => "FOR UPDATE OF r\n\nFOR UPDATE OF r"),
    /cited 1 time\(s\) and occurs on 2 line\(s\) \(1, 3\); quote a longer text/,
  );
  // Two citations of text that now occurs once: one guard was removed.
  assert.throws(() => resolveCitations(`${cite(1)} ${cite(5)}`, () => "FOR UPDATE OF r"), /cited 2 time\(s\) and occurs on 1 line/);
});

test("quoted text matches whole words at its ends", () => {
  const lock = cite(9, "lock(&mut tx).await?");
  // The catalog lock above is another guard: only the second line holds the text.
  assert.deepEqual(lines(resolveCitations(lock, () => "catalog_lock(&mut tx).await?;\n    lock(&mut tx).await?;")), [2]);
  assert.throws(() => resolveCitations(lock, () => "catalog_lock(&mut tx).await?;"), /no longer contains/);
  assert.throws(() => resolveCitations(cite(1, "fn accept_job"), () => "fn accept_job_v2("), /no longer contains/);
  assert.deepEqual(lines(resolveCitations(cite(3, ".request_timeout"), () => ".request_timeout\n.request_timeouts")), [1]);
});

test("empty inventories and malformed source citations fail closed", () => {
  assert.throws(() => resolveCitations("", () => ""), /no citations/);
  for (const malformed of [cite(2).replace("#L2", "#2"), cite(2).replaceAll("`", "")]) {
    assert.throws(
      () => resolveCitations(cite(2) + "\n" + malformed, () => "\nFOR UPDATE OF r"),
      /invalid citation/,
    );
  }
});

test("a reference-style source link cannot drop out beside a valid citation", () => {
  const target = "../../apps/gateway/src/governance/settlement.rs#L1";
  for (const label of ["`FOR UPDATE OF r`", "reservation lock"]) {
    const markdown = `${cite(2)}\n[${label}][guard]\n\n[guard]: <${target}>`;
    assert.throws(() => resolveCitations(markdown, () => "\nFOR UPDATE OF r"), /invalid citation/);
  }
});

test("renumbering rewrites only the line numbers", () => {
  const markdown = `Before ${cite(2)}, between ${cite(7, "state='pending'")} and after.`;
  const source = "\n\n\nFOR UPDATE OF r\nWHERE state='pending'";
  const fixed = renumber(markdown, resolveCitations(markdown, () => source));
  assert.equal(fixed, `Before ${cite(4)}, between ${cite(5, "state='pending'")} and after.`);
  assert.equal(renumber(fixed, resolveCitations(fixed, () => source)), fixed);
});

test("formal model README cites current implementation guards", () => {
  resolveCitations(readFileSync(README, "utf8"), readFromReadme);
});
