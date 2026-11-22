// Compose service images in the root and deploy stacks (#565).
import assert from "node:assert/strict";
import { test } from "node:test";
import { parseStrict, readLocalFromRepo, toData } from "./lib.mjs";
import { checkedComposeFiles, imageTrackingReference, isPinnedImage, upstreamComposeImages } from "./policy.mjs";

const staging = "deploy/staging/compose.yaml";
const loadtest = "deploy/loadtest/compose.yaml";
const candidate = "${GATEWAY_IMAGE:-open-model-gateway:staging}";
const loadImage = "${OMG_LOADTEST_IMAGE:-omg-loadtest:local}";

function violations(file, data) {
  return upstreamComposeImages(file, data)
    .filter(({ image }) => typeof image !== "string" || !isPinnedImage(image))
    .map(({ name }) => `${file}: services.${name}.image must be pinned by digest or use its approved local build with pull_policy: never`);
}

test("every upstream Compose service image in every tracked Compose file has a digest", () => {
  const files = [...checkedComposeFiles()];
  assert.deepEqual(files.flatMap(file => violations(file, toData(parseStrict(readLocalFromRepo(file), file)))), []);
});

test("fixtures distinguish pinned upstream defaults, local build outputs and overlays", () => {
  const pinned = "postgres:17-alpine@sha256:" + "a".repeat(64);
  const source = `x-db: &db {image: ${pinned}}\nservices:\n  db: {<<: *db}\n  app: {build: .}\n  overlay: {environment: {PROFILE: test}}\n`;
  assert.deepEqual(violations("compose.yml", toData(parseStrict(source, "compose.yml"))), []);
  const floating = source.replace(pinned, "postgres:17-alpine");
  assert.match(violations("compose.yml", toData(parseStrict(floating, "compose.yml"))).join(), /services\.db\.image/);
  const check = (file, name, service) => violations(file, { services: { [name]: service } });
  const digest = pinned.slice(pinned.indexOf("@"));
  for (const name of ["registry.example:5000/team/db:V1.0", "library/db", "[::1]:5000/team/db:dev"]) {
    assert.deepEqual(check("compose.yml", "db", { image: name + digest }), []);
  }
  for (const name of ["postgres:", "/postgres", "library//db", "registry:abc/team/db", "postgres:.bad"]) {
    assert.match(check("compose.yml", "db", { image: name + digest }).join(), /pinned by digest/, name);
  }
  const local = { image: candidate, pull_policy: "never" };
  assert.deepEqual(check(staging, "gateway", local), []);
  assert.deepEqual(check(loadtest, "gateway-cli", { image: loadImage, pull_policy: "never" }), []);
  const wrapped = "${STAGING_POSTGRES_IMAGE:-" + pinned + "}";
  assert.deepEqual(check(staging, "postgres", { image: wrapped }), []);
  for (const [file, name, service] of [
    ...["postgres", "postgres:17-alpine", pinned.slice(0, -1), pinned + ":extra", "${IMAGE}", null, [pinned]].map(image => ["compose.yml", "db", { image }]),
    ["compose.yml", "db", { image: "postgres:17", build: "." }],
    [staging, "postgres", { image: "${STAGING_POSTGRES_IMAGE:-postgres:17}" }],
    [staging, "gateway", { ...local, build: { context: "../..", dockerfile: "Dockerfile" } }],
    [loadtest, "loadgen", { image: loadImage }],
    [loadtest, "extra", { image: loadImage, pull_policy: "never" }],
    ["compose.yml", "loadgen", { image: loadImage, pull_policy: "never" }],
    ["compose.yml", "postgres", { image: wrapped }],
  ]) assert.equal(check(file, name, service).length, 1, `${file} ${name} ${JSON.stringify(service)}`);
});

test("pinned image references limit the normalized repository path to 255 characters", () => {
  const suffix = ":V1@sha256:" + "a".repeat(64);
  // The registry is outside the path. Docker Hub expands familiar names under library/.
  for (const [prefix, pathBudget] of [
    ["registry.example:5000/", 255], ["localhost/", 255], ["UpperHost/", 255],
    ["team/", 250], ["", 247], ["docker.io/", 247], ["index.docker.io/", 247],
  ]) {
    const reference = length => ({ services: { db: { image: prefix + "a".repeat(length) + suffix } } });
    assert.deepEqual(violations("compose.yml", reference(pathBudget)), [], prefix);
    assert.equal(violations("compose.yml", reference(pathBudget + 1)).length, 1, prefix);
  }
});

test("a new tagged upstream enters the Compose freshness inventory", () => {
  const image = "redis:7@sha256:" + "a".repeat(64);
  const data = { services: { cache: { image }, envOverlay: { environment: { LOG_LEVEL: "info" } } } };
  assert.deepEqual(upstreamComposeImages("deploy/cache/compose.yaml", data), [{ name: "cache", image }]);
});

test("staging uses the reference's tag before its digest-only fallback", () => {
  const digest = "@sha256:" + "a".repeat(64);
  const name = "registry.example:5000/team/postgres";
  assert.equal(imageTrackingReference(name + ":18" + digest, "17-bookworm"), name + ":18");
  assert.equal(imageTrackingReference(name + digest, "17-bookworm"), name + ":17-bookworm");
  assert.equal(imageTrackingReference(name + digest), undefined);
});
