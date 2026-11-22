import { pathToFileURL } from "node:url";
import { pinViolations } from "../tests/supply-chain/action-pins.mjs";
import { repoFiles } from "../tests/supply-chain/lib.mjs";

async function github(path) {
  const headers = { Accept: "application/vnd.github+json" };
  if (process.env.GH_TOKEN) headers.Authorization = `Bearer ${process.env.GH_TOKEN}`;
  const response = await fetch(`https://api.github.com${path}`, {
    headers, redirect: "error", signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok) throw new Error(`GET ${path} returned HTTP ${response.status}`);
  return response.json();
}

async function resolveTag(repo, tag, get) {
  const base = `/repos/${repo}/git`;
  const ref = await get(`${base}/ref/tags/${tag}`);
  if (ref.ref !== `refs/tags/${tag}`) throw new Error("unexpected tag reference");
  let object = ref.object;
  const seen = new Set();
  while (object && /^[0-9a-f]{40}$/.test(object.sha)) {
    if (object.type === "commit") return object.sha;
    if (object.type !== "tag" || seen.has(object.sha)) break;
    seen.add(object.sha);
    const annotated = await get(`${base}/tags/${object.sha}`);
    if (annotated.sha !== object.sha) throw new Error("unexpected annotated tag object");
    object = annotated.object;
  }
  throw new Error("tag did not resolve to a commit");
}

export async function verifyActionTags(files, get = github) {
  const pins = [];
  const errors = files.flatMap(({ text, file }) => pinViolations(text, file, pins));
  if (errors.length) throw new Error(errors.join("\n"));
  const resolved = new Map();
  for (const { repo, tag, sha, file } of pins) {
    const key = `${repo}@${tag}`;
    try {
      if (!resolved.has(key)) resolved.set(key, await resolveTag(repo, tag, get));
      const commit = resolved.get(key);
      if (commit !== sha) throw new Error(`mismatch: pinned ${sha}, upstream commit ${commit}`);
    } catch (error) {
      throw new Error(`${file}: ${key}: ${error.message}`, { cause: error });
    }
  }
  return resolved.size;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  console.log(`Verified ${await verifyActionTags(repoFiles())} upstream action tags`);
}
