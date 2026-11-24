// What secret-scan-extras.py writes for `gitleaks dir` (#603): commit and tag messages, file
// names, archive blobs under a name gitleaks opens, and UTF-16 blobs decoded to UTF-8; what it
// leaves out; and that it fails, never quietly covers less, on a git error or a passed bound.
// Its detection end to end, with the real scanner, is in secret-scan-run.test.mjs.
import assert from "node:assert/strict";
import { spawnSync } from "../process.mjs";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { root } from "./lib.mjs";
import { EXTRAS, commitAll, git, gitEnv } from "./secret-scan-lib.mjs";

function repo() {
  const base = mkdtempSync(join(tmpdir(), "secret-scan-extras-"));
  const dir = join(base, "repo");
  mkdirSync(dir);
  git(dir, "init", "-q", "-b", "main");
  writeFileSync(join(dir, "start.txt"), "start\n");
  const start = commitAll(dir, "start");
  return { base, dir, start, out: join(base, "out"), cleanup: () => rmSync(base, { recursive: true, force: true }) };
}
const run = (r, args, env = {}) => spawnSync("python3", ["-I", join(root, EXTRAS), r.out, ...args], { cwd: r.dir, encoding: "utf8", env: { ...gitEnv(), ...env } });
const read = (r, path) => readFileSync(join(r.out, path), "utf8");
const list = (r, sub) => readdirSync(join(r.out, sub)).sort();
const withRepo = fn => { const r = repo(); try { fn(r); } finally { r.cleanup(); } };

test("messages and paths of the range only, merge commits included", () => withRepo(r => {
  git(r.dir, "checkout", "-q", "-b", "side");
  writeFileSync(join(r.dir, "side.txt"), "side\n");
  commitAll(r.dir, "side work");
  git(r.dir, "checkout", "-q", "main");
  writeFileSync(join(r.dir, "main.txt"), "main\n");
  commitAll(r.dir, "main work");
  git(r.dir, "merge", "--no-ff", "-q", "-m", "merge message", "side");
  git(r.dir, "rm", "-q", "start.txt");
  const head = commitAll(r.dir, "drop start");
  const done = run(r, [`${r.start}..${head}`]);
  assert.equal(done.status, 0, done.stderr);
  const messages = read(r, "messages.txt");
  for (const m of ["side work", "main work", "merge message", "drop start"]) assert.match(messages, new RegExp(`\\n${m}\\n`));
  assert.doesNotMatch(messages, /\nstart\n/, "the base's own commit is outside the range");
  assert.equal(read(r, "names.txt"), "main.txt\nside.txt\n", "added paths only; a deletion adds no content");
  assert.equal(existsSync(join(r.out, "tags.txt")), false);
  assert.match(done.stderr, /4 commit messages, 2 paths, 0 archives, 0 UTF-16 files/);
}));

test("tag messages with --tag-messages", () => withRepo(r => {
  git(r.dir, "tag", "-a", "v1", "-m", "first release");
  git(r.dir, "tag", "light");
  assert.equal(run(r, ["--tag-messages", "HEAD", "--tags"]).status, 0);
  assert.match(read(r, "tags.txt"), /tag v1\nfirst release\n/);
}));

test("archives are written under a name gitleaks opens; .tgz becomes .tar.gz; other blobs are not written", () => withRepo(r => {
  for (const name of ["a.zip", "b.jar", "c.tar", "d.tar.gz", "e.gz", "f.tgz", "G.TGZ", "plain.txt", "image.png"]) writeFileSync(join(r.dir, name), `${name}\n`);
  const head = commitAll(r.dir, "archives");
  assert.equal(run(r, [`${r.start}..${head}`]).status, 0);
  assert.deepEqual(list(r, "archives").map(n => n.replace(/^\d+-/, "")), ["G.tar.gz", "a.zip", "b.jar", "c.tar", "d.tar.gz", "e.gz", "f.tar.gz"]);
  assert.deepEqual(list(r, "utf16"), []);
}));

test("UTF-16 blobs, with or without a byte order mark, are decoded; ASCII and binary are not", () => withRepo(r => {
  const text = "aws = made-up value\n";
  writeFileSync(join(r.dir, "le-bom.txt"), Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(text, "utf16le")]));
  writeFileSync(join(r.dir, "le.txt"), Buffer.from(text, "utf16le"));
  writeFileSync(join(r.dir, "be-bom.txt"), Buffer.concat([Buffer.from([0xfe, 0xff]), Buffer.from(text, "utf16le").swap16()]));
  writeFileSync(join(r.dir, "be.txt"), Buffer.from(text, "utf16le").swap16());
  writeFileSync(join(r.dir, "ascii.txt"), text);
  writeFileSync(join(r.dir, "binary.bin"), Buffer.from(Array.from({ length: 600 }, (_, i) => (i * 37 + 11) % 256)));
  const head = commitAll(r.dir, "wide");
  assert.equal(run(r, [`${r.start}..${head}`]).status, 0);
  const files = list(r, "utf16");
  assert.deepEqual(files.map(n => n.replace(/^\d+-/, "")), ["be-bom.txt.txt", "be.txt.txt", "le-bom.txt.txt", "le.txt.txt"]);
  for (const f of files) assert.equal(read(r, `utf16/${f}`), text);
}));

test("paths with a tab, a newline or non-ASCII characters keep their names and extensions", () => withRepo(r => {
  const names = ["tab\tname.zip", "new\nline.tgz", "café.txt"];
  for (const name of names) writeFileSync(join(r.dir, name), "x\n");
  writeFileSync(join(r.dir, "wide\tone.txt"), Buffer.from("made up\n", "utf16le"));
  const head = commitAll(r.dir, "odd names");
  const done = run(r, [`${r.start}..${head}`]);
  assert.equal(done.status, 0, done.stderr);
  assert.equal(read(r, "names.txt"), [...names, "wide\tone.txt"].sort().map(n => `${n}\n`).join(""));
  assert.deepEqual(list(r, "archives").map(n => n.replace(/^\d+-/, "")).sort(), ["new\nline.tar.gz", "tab\tname.zip"]);
  assert.deepEqual(list(r, "utf16").map(n => n.replace(/^\d+-/, "")), ["wide\tone.txt.txt"]);
}));

test("the same bytes under an archive name and a text name are still opened as an archive", () => withRepo(r => {
  writeFileSync(join(r.dir, "a.txt"), "same bytes\n");
  writeFileSync(join(r.dir, "z.zip"), "same bytes\n");
  const head = commitAll(r.dir, "twins");
  assert.equal(run(r, [`${r.start}..${head}`]).status, 0);
  assert.deepEqual(list(r, "archives").map(n => n.replace(/^\d+-/, "")), ["z.zip"]);
}));

test("a submodule entry (gitlink) has no blob and is skipped", () => withRepo(r => {
  git(r.dir, "update-index", "--add", "--cacheinfo", `160000,${r.start},sub`);
  git(r.dir, "commit", "-q", "-m", "submodule");
  const done = run(r, [`${r.start}..HEAD`]);
  assert.equal(done.status, 0, done.stderr);
  assert.match(done.stderr, /0 archives, 0 UTF-16 files/);
}));

test("a git error, missing revisions or an existing output fail", () => withRepo(r => {
  const bad = run(r, ["no-such-ref"]);
  assert.equal(bad.status, 2);
  assert.match(bad.stderr, /git log failed/);
  rmSync(r.out, { recursive: true, force: true });
  assert.equal(run(r, []).status, 2);
  mkdirSync(r.out, { recursive: true });
  assert.notEqual(run(r, ["HEAD"]).status, 0);
}));

test("a blob or a total over the bound fails instead of being skipped; the bounds can only be lowered", () => withRepo(r => {
  writeFileSync(join(r.dir, "big.zip"), "x".repeat(200));
  writeFileSync(join(r.dir, "big2.zip"), "y".repeat(200));
  const head = commitAll(r.dir, "big");
  const blob = run(r, [`${r.start}..${head}`], { SECRET_SCAN_EXTRAS_MAX_BLOB: "100" });
  assert.equal(blob.status, 3);
  assert.match(blob.stderr, /over the 100-byte bound/);
  rmSync(r.out, { recursive: true, force: true });
  const total = run(r, [`${r.start}..${head}`], { SECRET_SCAN_EXTRAS_MAX_TOTAL: "300" });
  assert.equal(total.status, 3);
  assert.match(total.stderr, /pass the 300-byte bound/);
  rmSync(r.out, { recursive: true, force: true });
  assert.equal(run(r, [`${r.start}..${head}`], { SECRET_SCAN_EXTRAS_MAX_BLOB: String(10 ** 12) }).status, 0);
  const source = readFileSync(join(root, EXTRAS), "utf8");
  assert.match(source, /return min\(default, int\(value\)\)/, "an override only lowers a bound");
}));

test("the script only reads git objects: no subprocess but git, nothing from the repository run", () => {
  const source = readFileSync(join(root, EXTRAS), "utf8");
  const calls = [...source.matchAll(/subprocess\.(run|Popen)\(\[([^\]]*)/g)].map(m => m[2].split(",")[0].trim());
  assert.deepEqual([...new Set(calls)], ['"git"']);
  assert.doesNotMatch(source, /\b(exec|eval|import_module|__import__|os\.system|shell=True)\b/);
});
