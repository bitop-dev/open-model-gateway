import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { spawnSync } from "../process.mjs";
import { loadWorkflow, root } from "./lib.mjs";
import { postgresServiceImages } from "./policy.mjs";

test("CI PostgreSQL service discovery follows the image repository", () => {
  const postgres = "mirror.example:5000/library/postgres:17@sha256:" + "a".repeat(64);
  assert.deepEqual(postgresServiceImages({ db: { image: postgres }, history: postgres, cache: { image: "postgres:5000/redis:7" } }),
    [["db", postgres], ["history", postgres]]);
});

test("workflow freshness checks the actual pins and tags and reports every affected job", t => {
  const scratch = mkdtempSync(join(tmpdir(), "image-pin-freshness-"));
  t.after(() => rmSync(scratch, { recursive: true, force: true }));

  // Serve fixture indexes through Buildx's CLI boundary without using a registry.
  const calls = join(scratch, "lookups");
  writeFileSync(join(scratch, "docker"), `#!/usr/bin/env node
const { appendFileSync, readFileSync } = require("node:fs");
const assert = require("node:assert/strict");
const argv = process.argv.slice(2);
assert.deepEqual(argv.slice(0, -1), ["buildx", "imagetools", "inspect", "--format", "{{json .Manifest}}"]);
const ref = argv.at(-1);
appendFileSync(${JSON.stringify(calls)}, ref + "\\n");
const postgres = "mirror.gcr.io/library/postgres:17-alpine";
if (process.env.FIXTURE_POSTGRES_FAILURE === "1" && ref.startsWith(postgres + "@")) {
  console.error("fixture registry unavailable");
  process.exit(1);
}
const builds = readFileSync(${JSON.stringify(calls)}, "utf8").split("\\n");
const pin = builds.find(value => value.startsWith(ref + "@"));
const digest = (ref.includes("@") ? ref : pin)?.split("@")[1];
const current = ref === postgres ? "sha256:" + "e".repeat(64) : ref === "mirror.gcr.io/docker/buildkit-syft-scanner:stable-1" ? "sha256:" + "f".repeat(64) : digest;
const manifests = ["amd64", "arm64"].map(architecture => ({platform: {os: "linux", architecture}}));
console.log(JSON.stringify({digest: current, manifests}));
`, { mode: 0o700 });

  function run(postgresFailure) {
    writeFileSync(calls, "");
    const result = spawnSync(process.execPath, ["scripts/check-image-pins.mjs"], {
      cwd: root, encoding: "utf8",
      env: { ...process.env, PATH: scratch + ":" + process.env.PATH, FIXTURE_POSTGRES_FAILURE: postgresFailure ? "1" : "0" },
    });
    assert.ifError(result.error);
    assert.equal(result.status, 1);
    assert.doesNotMatch(result.stdout + result.stderr, /fixture registry unavailable/);
    return result;
  }
  const result = run(false);
  for (const name of ["branch-images", "image", "loadtest-image", "ci"]) {
    assert.match(result.stdout, new RegExp(`workflows/${name}\\.yml.*mirror\\.gcr\\.io/moby/buildkit:buildx-stable-1 matches sha256:`));
  }
  for (const name of ["branch-images", "image", "loadtest-image"]) {
    assert.match(result.stderr, new RegExp(`workflows/${name}\\.yml.*mirror\\.gcr\\.io/docker/buildkit-syft-scanner:stable-1 moved from sha256:[a-f0-9]{64} to sha256:f{64}`));
  }
  const postgres = "mirror.gcr.io/library/postgres:17-alpine";
  const lookups = readFileSync(calls, "utf8").trim().split("\n");
  const workflowFiles = ["branch-images", "image", "loadtest-image", "ci"].map(name =>
    readFileSync(join(root, ".github/workflows", name + ".yml"), "utf8"));
  for (const tag of [
    "mirror.gcr.io/moby/buildkit:buildx-stable-1",
    "mirror.gcr.io/docker/buildkit-syft-scanner:stable-1",
    postgres,
  ]) {
    const declared = new Set();
    for (const workflow of workflowFiles) {
      for (const suffix of workflow.split(tag + "@").slice(1)) {
        const digest = /^sha256:[a-f0-9]{64}/.exec(suffix);
        assert.ok(digest, tag);
        declared.add(tag + "@" + digest[0]);
      }
    }
    assert.ok(declared.size > 0, tag);
    for (const ref of [tag, ...declared]) assert.equal(lookups.filter(value => value === ref).length, 1, ref);
  }

  const failed = run(true);
  for (const job of ["gateway-tests", "gateway-special", "browser"]) {
    const [service, config] = Object.entries(loadWorkflow("ci").jobs[job].services)
      .find(([, entry]) => (entry.image ?? entry).startsWith(postgres + "@"));
    const pinned = config.image ?? config;
    const where = `.github/workflows/ci.yml job '${job}' services.${service}`;
    assert.ok(result.stderr.split("\n").includes(`${where}: ${postgres} moved from ${pinned.split("@")[1]} to sha256:${"e".repeat(64)}`), where);
    assert.ok(failed.stderr.split("\n").includes(`${where}: lookup exited 1 for ${pinned}`), where);
  }
});

test("scheduled and manual image freshness uses read-only permissions without persisted credentials", () => {
  const workflow = loadWorkflow("image-pin-freshness");
  assert.deepEqual(Object.keys(workflow.on).sort(), ["schedule", "workflow_dispatch"]);
  assert.deepEqual(workflow.permissions, { contents: "read" });
  assert.deepEqual(Object.keys(workflow.jobs), ["check"]);
  const job = workflow.jobs.check;
  assert.equal(job.permissions, undefined);
  assert.equal(job.environment, undefined);
  assert.equal(job.env, undefined);
  assert.equal(workflow.env, undefined);
  const [checkout, ...steps] = job.steps;
  assert.match(checkout.uses, /^actions\/checkout@[a-f0-9]{40}$/);
  assert.deepEqual(checkout.with, { "persist-credentials": false });
  assert.deepEqual(steps, [
    { uses: "./.github/actions/setup-node" },
    { run: "npm ci --ignore-scripts" },
    { run: "node scripts/check-image-pins.mjs" },
  ]);
});
