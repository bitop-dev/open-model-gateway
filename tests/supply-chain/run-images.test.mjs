import assert from "node:assert/strict";
import { test } from "node:test";
import { imageBuildViolations } from "./policy.mjs";

const pinned = "example.invalid/probe:1@sha256:" + "a".repeat(64);
const documents = {
  workflow: run => ({ jobs: { check: { steps: [{ run }] } } }),
  action: run => ({ runs: { using: "composite", steps: [{ shell: "bash", run }] } }),
};
for (const [kind, document] of Object.entries(documents)) {
  const check = run => imageBuildViolations(kind, document(run), { pinned: new Set(), composeFiles: new Set(["compose.yaml"]) });
  for (const redirect of ["&>", "&>>"]) {
    test(`${kind} inventories Docker operands across ${redirect}`, () => {
      assert.deepEqual(check(`docker pull ${redirect}log ${pinned}`), []);
      assert.match(check(`docker ${redirect}log pull alpine`).join(), /digest/);
      assert.deepEqual(check(`docker run --name 2${redirect}log ${pinned}`), []);
    });
  }
  test(`${kind} ends Compose option parsing at the service`, () => {
    assert.deepEqual(check("docker compose run --rm -- gateway tool --build"), []);
    assert.deepEqual(check("docker compose run -e --build gateway"), []);
    assert.match(check("docker compose run --rm --build gateway").join(), /builds an image/);
  });
  test(`${kind} preserves literal option values and refuses uncertain boundaries`, () => {
    assert.deepEqual(check(`docker run --env 'PRICE=$PRICE' ${pinned}`), []);
    assert.match(check(`docker run --env PRICE=$PRICE ${pinned}`).join(), /digest/);
  });
  test(`${kind} inventories commands after a quoted cat heredoc`, () => {
    const data = "cat <<-'END' >out\n\t'unclosed literal\n\tdocker pull floating\n\t END\n\tEND\n";
    assert.deepEqual(check(data + `docker pull ${pinned}`), []);
    assert.match(check(data + "docker create floating").join(), /digest/);
    assert.deepEqual(check('cat <<"END"\ndocker pull floating\nEND\ntrue'), []);
  });
  test(`${kind} checks heredocs that can execute commands`, () => {
    assert.match(check("bash <<'END'\ndocker create floating\nEND").join(), /digest/);
    assert.throws(() => check("cat <<END\n'$(docker pull floating)'\nEND"), /heredoc/);
    assert.throws(() => check("cat <<'END' | sh\ndocker pull floating\nEND"), /heredoc/);
    assert.throws(() => check("cat <<-'END'\n END"), /heredoc/);
  });
  test(`${kind} refuses unpinned Docker operands`, () => {
    for (const run of [
      'docker run --rm alpine:latest true',
      `docker run --name $NAME ${pinned}`,
      'docker 2>/dev/null pull alpine',
      'docker image pull alpine',
      'docker container create alpine',
      'docker-compose run example.invalid/probe:1',
      'docker compose run --pull always gateway',
      'docker pu' + '\\' + '\nll alpine',
      `docker run -e ${pinned} alpine true`,
      'result=$(docker pull --platform linux/amd64 busybox)',
      'if docker create --name test "$IMAGE"; then true; fi',
      `docker create app@sha256:abc echo ${pinned}`,
      'docker pull "${IMAGE}@${DIGEST}"',
      'docker compose -f compose.yaml run --rm example.invalid/probe:1',
    ]) assert.match(check(run).join(), /digest|service name/, run);
  });
  test(`${kind} accepts literal pins and Compose service operands`, () => {
    for (const run of [
      `docker run --rm -it --name trial "${pinned}" alpine:latest`,
      `docker pull --platform=linux/amd64 ${pinned}`,
      `docker create --entrypoint /bin/true ${pinned}`,
      'docker compose -f compose.yaml run --rm --pull never -e MODE=test gateway echo alpine:latest',
      `docker run --name '>' ${pinned}`,
      'echo "docker pull alpine:latest" # docker run busybox',
    ]) assert.deepEqual(check(run), [], run);
  });
}

test("the release digest exception is scoped to the step, verb and exact operand", () => {
  const file = ".github/workflows/image.yml";
  const name = "Pull the pushed digest";
  const check = (step, run) => imageBuildViolations(file, {
    jobs: { verify: { steps: [{ name: step, run }] } },
  }, { pinned: new Set([file + ": job 'verify'"]), composeFiles: new Set() });
  const image = '"${IMAGE}@${DIGEST}"';
  assert.deepEqual(check(name, "docker pull " + image), []);
  for (const [step, run] of [
    ["Other step", "docker pull " + image],
    [name, "docker create " + image],
    [name, 'docker pull "${IMAGE}@${OTHER}"'],
    [name, "docker pull alpine:latest"],
  ]) assert.match(check(step, run).join(), /digest/);
  assert.match(check(name, "docker compose -f unknown.yml run service").join(), /checked Compose file/);
});
