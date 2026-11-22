// Report registry tag drift for the upstream Compose and chart defaults.
import { execFileSync } from "node:child_process";
import { parseStrict, readLocalFromRepo, root, toData } from "../tests/supply-chain/lib.mjs";
import { checkedComposeFiles, imageTrackingReference, isPinnedImage, upstreamComposeImages } from "../tests/supply-chain/policy.mjs";

const yaml = file => toData(parseStrict(readLocalFromRepo(file), file));
const chart = "deploy/helm/open-model-gateway/values.yaml";
const pins = [];
// These two staging defaults are digest-only. Other upstreams carry their tag.
const stagingTags = new Map([["postgres", "17-bookworm"], ["ingress", "2-alpine"]]);
for (const file of checkedComposeFiles()) {
  for (const { name, image } of upstreamComposeImages(file, yaml(file))) {
    const tag = file === "deploy/staging/compose.yaml" ? stagingTags.get(name) : undefined;
    pins.push([`${file} services.${name}`, image, tag]);
  }
}
pins.push([chart, yaml(chart).cnpg.cluster.image]);

const lookups = new Map();
function indexDigest(reference) {
  if (!lookups.has(reference)) {
    const index = JSON.parse(execFileSync("docker", [
      "buildx", "imagetools", "inspect", "--format", "{{json .Manifest}}", reference,
    ], { cwd: root, encoding: "utf8", timeout: 30_000 }));
    for (const architecture of ["amd64", "arm64"]) {
      if (!index.manifests?.some(m => m.platform?.os === "linux" && m.platform.architecture === architecture)) {
        throw new Error(`${reference} lacks a linux/${architecture} manifest`);
      }
    }
    if (!/^sha256:[a-f0-9]{64}$/.test(index.digest)) {
      throw new Error(`${reference} did not return an index digest`);
    }
    lookups.set(reference, index.digest);
  }
  return lookups.get(reference);
}

for (const [file, pinned, trackingTag] of pins) {
  try {
    if (typeof pinned !== "string" || !isPinnedImage(pinned)) throw new Error("expected a digest-pinned image");
    const [, digest] = pinned.split("@");
    const tag = imageTrackingReference(pinned, trackingTag);
    if (!tag) throw new Error(`${pinned} has no tracking tag`);
    if (indexDigest(pinned) !== digest) throw new Error(`${pinned} did not resolve to its pinned digest`);
    const current = indexDigest(tag);
    if (current !== digest) throw new Error(`${tag} moved from ${digest} to ${current}`);
    console.log(`${file}: ${tag} matches ${digest} (linux/amd64, linux/arm64)`);
  } catch (error) {
    console.error(`${file}: ${error.message}`);
    process.exitCode = 1;
  }
}
