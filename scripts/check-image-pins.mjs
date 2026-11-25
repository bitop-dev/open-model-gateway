// Report registry tag drift for upstream Compose, chart and workflow image pins.
import { spawnSync } from "node:child_process";
import { actionName, jobsOf, parseStrict, readLocalFromRepo, repoFiles, root, stepsOf, toData } from "../tests/supply-chain/lib.mjs";
import { checkedComposeFiles, imagePinLocation, imageTrackingReference, isPinnedImage, postgresServiceImages, upstreamComposeImages } from "../tests/supply-chain/policy.mjs";

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
pins.push([`${chart} cnpg.cluster.image`, yaml(chart).cnpg.cluster.image]);
pins.push([`${chart} cnpg.pooler.image`, yaml(chart).cnpg.pooler.image]);

for (const { file, text } of repoFiles()) {
  if (!file.startsWith(".github/workflows/")) continue;
  for (const [name, job] of jobsOf(toData(parseStrict(text, file)))) {
    const location = `${file} job '${name}'`;
    if (file === ".github/workflows/ci.yml") {
      for (const [service, image] of postgresServiceImages(job.services)) {
        pins.push([`${location} services.${service}`, image]);
      }
    }
    for (const step of stepsOf(job)) {
      const action = actionName(step.uses);
      if (action === "docker/setup-buildx-action") {
        const options = step.with?.["driver-opts"];
        const image = typeof options === "string" ? options.match(/^image=(\S+)$/m)?.[1] : undefined;
        pins.push([`${location} Buildx driver-opts image`, image]);
      }
      if (action === "docker/build-push-action" && typeof step.with?.attests === "string") {
        for (const line of step.with.attests.split("\n")) {
          const fields = line.split(",").map(field => field.trim());
          if (fields.includes("type=sbom")) {
            const generator = fields.find(field => field.startsWith("generator="))?.slice("generator=".length);
            pins.push([`${location} SBOM generator`, generator]);
          }
        }
      }
    }
  }
}

const lookups = new Map();
function indexDigest(reference) {
  if (!lookups.has(reference)) {
    const lookup = spawnSync("docker", [
      "buildx", "imagetools", "inspect", "--format", "{{json .Manifest}}", reference,
    ], { cwd: root, encoding: "utf8", timeout: 30_000, stdio: ["ignore", "pipe", "ignore"] });
    if (lookup.error || lookup.status !== 0) {
      const outcome = Number.isInteger(lookup.status) ? `exited ${lookup.status}` : "did not complete";
      throw new Error(`lookup ${outcome} for ${reference}`);
    }
    let index;
    try {
      index = JSON.parse(lookup.stdout);
    } catch {
      throw new Error(`${reference} returned invalid manifest JSON`);
    }
    for (const architecture of ["amd64", "arm64"]) {
      if (!Array.isArray(index?.manifests) || !index.manifests.some(m => m?.platform?.os === "linux" && m.platform.architecture === architecture)) {
        throw new Error(`${reference} lacks a linux/${architecture} manifest`);
      }
    }
    if (typeof index.digest !== "string" || !/^sha256:[a-f0-9]{64}$/.test(index.digest)) {
      throw new Error(`${reference} did not return an index digest`);
    }
    lookups.set(reference, index.digest);
  }
  return lookups.get(reference);
}

for (const [source, pinned, trackingTag] of pins) {
  const location = imagePinLocation(source);
  try {
    if (typeof pinned !== "string" || !isPinnedImage(pinned)) throw new Error("expected a digest-pinned image");
    const [, digest] = pinned.split("@");
    const tag = imageTrackingReference(pinned, trackingTag);
    if (!tag) throw new Error(`${pinned} has no tracking tag`);
    if (indexDigest(pinned) !== digest) throw new Error(`${pinned} did not resolve to its pinned digest`);
    const current = indexDigest(tag);
    if (current !== digest) throw new Error(`${tag} moved from ${digest} to ${current}`);
    console.log(`${location}: ${tag} matches ${digest} (linux/amd64, linux/arm64)`);
  } catch (error) {
    console.error(`${location}: ${error.message}`);
    process.exitCode = 1;
  }
}
