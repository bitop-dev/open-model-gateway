#!/usr/bin/env node
// Source citations of the formal model README (spec/formal/README.md).
//
//     node scripts/formal-citations.mjs          # check, and list citations whose line moved
//     node scripts/formal-citations.mjs --fix    # rewrite the line numbers to where the text is now
//
// `npm run test:rules` runs the same check (tests/formal-citations.test.mjs).
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

export const README = new URL("../spec/formal/README.md", import.meta.url);
const INLINE = /\[`([^`\n]+)`\]\((\.\.\/\.\.\/(?:apps|deploy)\/(?:[\w-]+\/)*[\w-]+\.(?:rs|sql))#L([1-9]\d*)\)/g;
const fail = message => { throw new Error(message); };

// The README is the inventory. A code-labelled link quotes the source, and a
// link into the gateway or deployment tree must carry a code label.
//
// A citation names a guard by its quoted text; the line number is where a
// reader finds it. The cited line wins while it still holds the text. When it
// does not, the text must still identify one line: among the lines that hold
// it and are not cited exactly, there are as many as there are moved citations
// of it in that file, taken in order. Text that is gone, or that occurs more
// often than it is cited, fails: the guard changed, or the citation no longer
// says which occurrence it means.
export function resolveCitations(markdown, readSource) {
  const citations = [...markdown.matchAll(INLINE)].map(match => ({
    text: match[0], token: match[1], path: match[2], cited: Number(match[3]), index: match.index, line: null,
  }));
  for (const citation of citations) if (!citation.token.trim()) fail(`invalid citation: ${citation.text}`);
  const remaining = markdown.replace(INLINE, "");
  // A leftover source path or code label signals a malformed or reference-style citation.
  if (/\[`|\.\.\/\.\.\/(?:apps|deploy)\//.test(remaining)) fail("invalid citation: use an inline source link");
  if (!citations.length) fail("no citations in formal README");
  const groups = new Map();
  for (const citation of citations) {
    const key = `${citation.path}\n${citation.token}`;
    groups.set(key, [...(groups.get(key) ?? []), citation]);
  }
  for (const group of groups.values()) {
    const { path, token } = group[0];
    const lines = readSource(path).split(/\r?\n/);
    const holding = lines.flatMap((line, i) => line.includes(token) ? [i + 1] : []);
    if (!holding.length) fail(`${path} no longer contains ${token}`);
    const moved = [];
    for (const citation of group) {
      if (holding.includes(citation.cited)) citation.line = citation.cited;
      else moved.push(citation);
    }
    const free = holding.filter(line => !group.some(citation => citation.line === line));
    if (moved.length && moved.length !== free.length) {
      fail(`${path}#L${moved[0].cited}: ${token} is cited ${group.length} time(s) and occurs on ${holding.length} line(s) (${holding.join(", ")}); quote a longer text that occurs once`);
    }
    moved.sort((a, b) => a.cited - b.cited).forEach((citation, i) => { citation.line = free[i]; });
  }
  return citations;
}

/// The README with every citation's line number where its text is now.
export function renumber(markdown, citations) {
  let out = "";
  let at = 0;
  for (const citation of [...citations].sort((a, b) => a.index - b.index)) {
    out += markdown.slice(at, citation.index) + citation.text.replace(/#L\d+\)$/, `#L${citation.line})`);
    at = citation.index + citation.text.length;
  }
  return out + markdown.slice(at);
}

export const readFromReadme = path => readFileSync(new URL(path, README), "utf8");

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const args = process.argv.slice(2);
    if (args.length > 1 || (args.length === 1 && args[0] !== "--fix")) fail("usage: formal-citations.mjs [--fix]");
    const markdown = readFileSync(README, "utf8");
    const citations = resolveCitations(markdown, readFromReadme);
    const stale = citations.filter(citation => citation.line !== citation.cited);
    for (const citation of stale) console.log(`${citation.path}#L${citation.cited} -> L${citation.line}: ${citation.token}`);
    if (args[0] === "--fix" && stale.length) writeFileSync(README, renumber(markdown, citations));
    console.log(`${citations.length} citations, ${stale.length} with a moved line${args[0] === "--fix" && stale.length ? " (rewritten)" : ""}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
