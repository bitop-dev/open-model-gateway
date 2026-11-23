import { isScalar, visit } from "yaml";
import { parseStrict } from "./lib.mjs";

// Every `uses:` anywhere in the file, read from the syntax tree so comments and
// flow-style or aliased values are seen as they are.
export function pinViolations(text, file, pins = []) {
  const doc = parseStrict(text, file);
  const out = [];
  visit(doc, {
    Pair(_, pair) {
      if (!isScalar(pair.key) || pair.key.value !== "uses") return;
      const v = pair.value;
      if (!isScalar(v) || typeof v.value !== "string") {
        out.push(`${file}: a 'uses' value is not a plain string (aliases and expressions cannot be checked)`);
        return;
      }
      const ref = v.value;
      if (ref.startsWith("./")) return; // local actions and reusable workflows
      if (ref.startsWith("docker://")) {
        if (!/@sha256:[0-9a-f]{64}$/.test(ref)) out.push(`${file}: ${ref} is not pinned by digest`);
        return;
      }
      const match = /^([A-Za-z0-9_-]+\/[A-Za-z0-9_-][A-Za-z0-9_.-]*)(?:\/[A-Za-z0-9_.-]+)*@([0-9a-f]{40})$/.exec(ref);
      const tag = (v.comment ?? "").trim();
      if (!match || !/^v[0-9]+(?:\.[0-9]+)*$/.test(tag)) {
        out.push(`${file}: ${ref} needs a full commit SHA and a version comment`);
        return;
      }
      pins.push({ file, repo: match[1], sha: match[2], tag });
    },
  });
  return out;
}
