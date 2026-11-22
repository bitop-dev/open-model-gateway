"""Local fake-tool contracts only; not real ORAS/BuildKit/transport approval.
Run: python3 -m unittest discover -s tests -p oci_stage_test.py -v
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

from oci_layout_test import fixture, blob_path

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "ghcr.io/bitop-dev/open-model-gateway"
COMMIT = "9b1fc815f6b7fba743f08b9b5c5846639cc11d3c"
TOKEN = "fixture-private-token"


class OciStageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        scripts = self.base / ".github/scripts"
        scripts.mkdir(parents=True)
        for name in ("oci-stage.py", "oci-layout.py"):
            shutil.copyfile(ROOT / ".github/scripts" / name, scripts / name)
        self.script = scripts / "oci-stage.py"
        spec = importlib.util.spec_from_file_location("oci_stage_fixture", self.script)
        self.api = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.api)
        self.config = self.base / ".github/tools/oras.json"
        self.config.parent.mkdir()
        self.tool, self.log = self.base / "fake-oras", self.base / "calls.jsonl"
        self.install_tool()
        self.graph = fixture()
        self.archive = self.base / "archive.tar"
        self.write_archive()
        self.env = dict(os.environ, GITHUB_TOKEN=TOKEN, GITHUB_ACTOR="fixture-actor",
                        HTTPS_PROXY="https://credential.invalid", http_proxy="proxy-secret",
                        ALL_PROXY="proxy-secret", NO_PROXY="ambient", DOCKER_CONFIG="helper-secret",
                        DOCKER_AUTH_CONFIG="auth-secret", GH_TOKEN="gh-secret",
                        ACTIONS_ID_TOKEN_REQUEST_TOKEN="oidc-secret", PATH="/ambient/helpers",
                        HOME=str(self.base / "ambient-home"), TMPDIR=str(self.base / "ambient-tmp"))

    def install_tool(self, fail="", sleep="", diagnostic=None):
        self.tool.write_text(f'''#!{sys.executable}
import hashlib, json, os, pathlib, stat, sys, time
args = sys.argv[1:]
kind = args[0]
auth = pathlib.Path(args[args.index('--registry-config' if kind == 'login' else '--to-registry-config') + 1])
record = dict(argv=args, env=dict(os.environ), stdin=sys.stdin.read(), auth=str(auth),
              config=json.loads(auth.read_text()), mode=stat.S_IMODE(auth.stat().st_mode),
              parent_mode=stat.S_IMODE(auth.parent.stat().st_mode), cwd=os.getcwd())
if kind == 'cp':
    record['archive_sha256'] = hashlib.sha256(pathlib.Path(args[-2].rsplit('@', 1)[0]).read_bytes()).hexdigest()
with open({str(self.log)!r}, 'a') as log:
    log.write(json.dumps(record) + '\\n')
if kind == {sleep!r}:
    time.sleep(30)
print('untrusted-output fixture-private-token')
if kind == 'cp' and {diagnostic is not None!r}:
    sys.stderr.buffer.write({diagnostic!r})
else:
    print('credentialed-error fixture-private-token', file=sys.stderr)
if kind == {fail!r}:
    sys.exit(1)
if kind == 'login':
    auth.write_text(json.dumps({{'auths': {{'ghcr.io': {{'auth': 'fixture-credential'}}}}}}))
''')
        self.tool.chmod(0o700)
        self.pin = dict(version="1.3.6", source_commit=COMMIT,
                        binary_sha256=hashlib.sha256(self.tool.read_bytes()).hexdigest())
        self.config.write_text(json.dumps(self.pin))

    def write_archive(self):
        with tarfile.open(self.archive, "w", format=tarfile.USTAR_FORMAT) as archive:
            for name, data in self.graph["files"].items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))

    def args(self, **changes):
        values = dict(archive=str(self.archive), root=self.graph["root"]["digest"],
                      platform="linux/amd64", image=IMAGE, tool=str(self.tool))
        values.update(changes)
        return [part for key, value in values.items() for part in ("--" + key, value)]

    def invoke(self, args=None):
        out, err, directories = io.StringIO(), io.StringIO(), []
        temporary = tempfile.TemporaryDirectory
        def private(*args, **kwargs):
            directory = temporary(*args, **kwargs)
            directories.append(Path(directory.name))
            self.assertEqual(directories[-1].stat().st_mode & 0o777, 0o700)
            return directory
        with mock.patch.dict(os.environ, self.env, clear=True), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err), mock.patch.object(self.api.tempfile, "TemporaryDirectory", side_effect=private):
            code = self.api.main(self.args() if args is None else args)
        self.assertTrue(all(not path.exists() for path in directories))
        self.assertNotIn(TOKEN, out.getvalue() + err.getvalue())
        return code, out.getvalue(), err.getvalue()

    def records(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def assert_cleanup(self):
        for record in self.records():
            for path in (record["auth"], record["env"]["HOME"], record["cwd"]):
                self.assertFalse(Path(path).exists(), path)

    def refused(self, args=None, calls=0, diagnostic=None):
        boundary = mock.patch.object(self.api.subprocess, "run", side_effect=AssertionError("preflight executed a process")) if not calls else contextlib.nullcontext()
        with boundary:
            code, out, err = self.invoke(args)
        self.assertNotEqual(code, 0)
        self.assertEqual(out, "")
        if diagnostic is None:
            self.assertIn(err, {"oci-stage: validation failed\n", "oci-stage: authentication unavailable\n"})
        else:
            self.assertEqual(err, f"oci-stage: {diagnostic}\n")
        self.assertEqual(len(self.records()), calls)
        self.assert_cleanup()

    def test_exact_digest_only_copy_for_both_platforms_and_repositories(self):
        for arch, image in (("amd64", IMAGE), ("arm64", IMAGE + "-loadtest")):
            with self.subTest(arch=arch):
                self.graph = fixture(arch)
                self.write_archive()
                original = self.archive.read_bytes()
                code, out, err = self.invoke(self.args(platform="linux/" + arch, image=image))
                self.assertEqual((code, out, err), (0, self.graph["root"]["digest"] + "\n", ""))
                login, copy = self.records()[-2:]
                auth = login["auth"]
                self.assertEqual(login["argv"], ["login", "--registry-config", auth, "--username", "fixture-actor", "--password-stdin", "ghcr.io"])
                self.assertEqual(copy["argv"][:-2], ["cp", "--force", "--no-tty", "--from-oci-layout", "--to-registry-config", auth, "--to-distribution-spec", "v1.1-referrers-api"])
                self.assertEqual(copy["argv"][-1], image)
                source, root = copy["argv"][-2].rsplit("@", 1)
                self.assertEqual(root, self.graph["root"]["digest"])
                self.assertEqual(copy["stdin"], "")
                self.assertEqual(login["stdin"], TOKEN)
                self.assertEqual(login["config"], {"auths": {}})
                self.assertEqual(copy["config"], {"auths": {"ghcr.io": {"auth": "fixture-credential"}}})
                self.assertTrue(Path(source).is_absolute())
                self.assertEqual(self.archive.read_bytes(), original)
                self.assertEqual(copy["archive_sha256"], hashlib.sha256(original).hexdigest())
                for record in (login, copy):
                    self.assertEqual(record["mode"], 0o600)
                    self.assertEqual(record["parent_mode"], 0o700)
                    # macOS/Python may synthesize these locale variables after exec.
                    self.assertLessEqual(set(record["env"]), {"HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "__CF_USER_TEXT_ENCODING"})
                    self.assertNotEqual(record["cwd"], str(self.base))
                self.assert_cleanup()

    def test_cli_success_is_only_the_root_digest(self):
        self.install_tool(diagnostic=b"Error response from registry: denied: fixture-private-token\n")
        result = subprocess.run([sys.executable, str(self.script), *self.args()], env=self.env, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, (self.graph["root"]["digest"] + "\n").encode())
        self.assertEqual(result.stderr, b"")
        self.assert_cleanup()

    def test_cli_copy_failure_has_no_digest_or_transport_text(self):
        self.install_tool(fail="cp", diagnostic=b"Error response from registry: denied: fixture-private-token\n")
        result = subprocess.run([sys.executable, str(self.script), *self.args()], env=self.env, capture_output=True, timeout=10)
        self.assertEqual((result.returncode, result.stdout, result.stderr),
                         (1, b"", b"oci-stage: copy failed (registry-access)\n"))
        self.assertEqual(len(self.records()), 2)
        self.assert_cleanup()

    def test_missing_production_pin_is_fail_closed(self):
        self.config.unlink()
        self.refused()

    def test_invalid_pin_fields_and_duplicate_keys_refuse_before_execution(self):
        for field, value in (("version", "1.3.5"), ("source_commit", "f" * 40),
                             ("binary_sha256", "f" * 64), ("binary_sha256", "XYZ"), ("extra", "override")):
            with self.subTest(field=field, value=value):
                self.config.write_text(json.dumps(dict(self.pin, **{field: value})))
                self.refused()
        self.config.write_text('{"version":"wrong",' + json.dumps(self.pin)[1:])
        self.refused()

    def test_tool_integrity_and_execute_permission_are_required(self):
        self.tool.write_text(self.tool.read_text() + "\n# changed")
        self.refused()
        self.install_tool()
        self.tool.chmod(0o600)
        self.refused()

    def test_symlink_directory_and_fifo_tool_and_config_are_refused(self):
        for path in (self.tool, self.config):
            saved = path.read_bytes()
            mode = path.stat().st_mode
            for kind in ("symlink", "directory", "fifo"):
                with self.subTest(path=path.name, kind=kind):
                    path.unlink()
                    target = self.base / "target"
                    target.write_bytes(saved)
                    if kind == "symlink":
                        path.symlink_to(target)
                    elif kind == "directory":
                        path.mkdir()
                    else:
                        os.mkfifo(path)
                    self.refused()
                    path.rmdir() if kind == "directory" else path.unlink()
                    path.write_bytes(saved)
                    path.chmod(mode)

    def test_archive_symlink_directory_and_fifo_are_refused(self):
        saved = self.archive.read_bytes()
        target = self.base / "archive-target"
        target.write_bytes(saved)
        for kind in ("symlink", "directory", "fifo"):
            self.archive.unlink()
            if kind == "symlink":
                self.archive.symlink_to(target)
            elif kind == "directory":
                self.archive.mkdir()
            else:
                os.mkfifo(self.archive)
            self.refused()
            self.archive.rmdir() if kind == "directory" else self.archive.unlink()
            self.archive.write_bytes(saved)

    def test_invalid_archive_root_platform_and_evidence_never_authenticate(self):
        for change in ("tar", "blob", "evidence", "root", "platform"):
            with self.subTest(change=change):
                self.graph = fixture()
                if change == "blob":
                    self.graph["files"][blob_path(self.graph["descriptors"]["layer"])] = b"bad"
                if change == "evidence":
                    self.graph = fixture(edits={"spdx": lambda x: dict(x, subject=[])})
                self.write_archive()
                if change == "tar":
                    self.archive.write_bytes(b"not a tar")
                kwargs = {"root": "sha256:" + "0" * 64} if change == "root" else {"platform": "linux/arm64"} if change == "platform" else {}
                with mock.patch.object(os.environ, "get", wraps=os.environ.get) as reads:
                    self.refused(self.args(**kwargs))
                self.assertFalse({"GITHUB_TOKEN", "GITHUB_ACTOR"} & {call.args[0] for call in reads.call_args_list})

    def test_repositories_refs_urls_platforms_extra_args_and_abbreviations_are_rejected(self):
        for image in ("ghcr.io/ncecere/open-model-gateway", "ghcr.io/ncecere/open-model-gateway-loadtest", IMAGE + ":tag", IMAGE + "@sha256:" + "f" * 64, "https://" + IMAGE, IMAGE + "/child", "ghcr.io/other/repo", "--help"):
            self.refused(self.args(image=image))
        self.refused(self.args(platform="linux/s390x"))
        self.refused(self.args(root="sha256:" + "A" * 64))
        self.refused(self.args() + ["--override", "anything"])
        self.refused(["--arch" if x == "--archive" else x for x in self.args()])

    def test_missing_credentials_never_execute_the_tool(self):
        for name in ("GITHUB_TOKEN", "GITHUB_ACTOR"):
            with mock.patch.dict(self.env, {name: ""}):
                self.refused()

    def test_login_and_copy_failures_are_redacted_and_cleaned(self):
        for kind, count, diagnostic in (("login", 1, "login failed"), ("cp", 2, "copy failed (unknown)")):
            with self.subTest(kind=kind):
                self.log.unlink(missing_ok=True)
                self.install_tool(fail=kind)
                self.refused(calls=count, diagnostic=diagnostic)

    def test_copy_failure_categories_are_bounded_and_contain_no_transport_text(self):
        registry = b"Error response from registry: "
        destination = f'Error from destination registry for "{IMAGE}": '.encode()
        url = b'"https://ghcr.io/v2/bitop-dev/open-model-gateway/blobs/uploads/?fixture-secret=query"'
        fixtures = (
            (registry + b"denied: fixture-private-token", "registry-access"),
            (destination + b"unauthorized", "registry-access"),
            (destination + b"POST " + url + b": response status code 403: fixture-private-token", "registry-access"),
            (registry + b"toomanyrequests: slow down", "registry-rate-limit"),
            (destination + b"POST " + url + b": response status code 429: Too Many Requests", "registry-rate-limit"),
            (registry + b"blob upload invalid: detail", "registry-content"),
            (registry + b"blob upload unknown: detail", "registry-content"),
            (registry + b"manifest unknown: detail", "registry-content"),
            (destination + b"PUT " + url + b": EOF", "transport"),
            (destination + b"PUT " + url + b": unexpected EOF", "transport"),
            (destination + b'HEAD ' + url + b': Get "https://ghcr.io/token?fixture=secret": EOF', "transport"),
            (destination + b'HEAD ' + url + b': Get "https://other.invalid/token": EOF', "unknown"),
            (destination + b"PUT " + url + b": EOF in opaque message", "unknown"),
            (registry + b"manifest blob unknown: fixture-private-token", "registry-content"),
            (destination + b"PUT " + url + b": response status code 503: Service Unavailable", "registry-server"),
            (destination + b"Head " + url + b": dial tcp: lookup ghcr.io: no such host", "transport"),
            (destination + b"Head " + url + b": net/http: TLS handshake timeout", "transport"),
            (registry + b"unknown: response status code 403: fixture-private-token", "unknown"),
            (registry + b"unauthorized-ish: fixture-private-token", "unknown"),
            (destination + b'POST "https://ghcr.io.example/v2/": response status code 429: fake', "unknown"),
            (f'Error from destination registry for "{IMAGE}-loadtest": denied'.encode(), "unknown"),
            (b"Error from source oci-layout: " + registry + b"denied: fixture-private-token", "unknown"),
            (b"unrelated error\n" + registry + b"denied: fixture-private-token", "unknown"),
            (registry + b"denied: " + b"x" * 65536, "unknown"),
            (b"\xff\x00::error::fixture-private-token", "unknown"),
        )
        for index, (stderr, category) in enumerate(fixtures):
            with self.subTest(index=index, category=category):
                self.log.unlink(missing_ok=True)
                self.install_tool(fail="cp", diagnostic=stderr + b"\n")
                self.refused(calls=2, diagnostic=f"copy failed ({category})")

    def test_copy_diagnostics_after_registry_warnings(self):
        warning = (b"[2026-11-22T08:12:08.123Z][WARNING]: fixture text\n"
                   b"[Data]:\n  registry=ghcr.io\n\n\n")
        error = b"Error response from registry: toomanyrequests: fixture detail\n"
        for report, category in (
            (warning + error, "registry-rate-limit"),
            (warning * 2 + error, "registry-rate-limit"),
            (warning.replace(b"fixture text", b"line one\nline two") + error, "unknown"),
            (warning + error + b"Error: another failure\n", "unknown"),
            (warning * 4 + error, "unknown"),
            (warning + error + b"x" * 4096, "unknown"),
        ):
            self.assertEqual(self.api.copy_failure_category(io.BytesIO(report), IMAGE), category)

    def test_copy_process_outcome_precedes_stderr(self):
        def signaled(args, **kwargs):
            kwargs["stderr"].write(b"Error response from registry: denied\n")
            return subprocess.CompletedProcess(args, -15)
        for effect, label in ((signaled, "signal"), (FileNotFoundError(), "execution"),
                              (lambda args, **kw: subprocess.CompletedProcess(args, 9), "unknown")):
            with mock.patch.object(self.api.subprocess, "run", side_effect=effect):
                with self.assertRaisesRegex(self.api.Refusal, rf"^copy failed \({label}\)$"):
                    self.api.run(self.tool, [], {}, self.base, "copy failed", copy_image=IMAGE)

    def test_login_and_copy_timeouts_are_bounded_redacted_and_cleaned(self):
        run = subprocess.run
        for kind, count, diagnostic in (("login", 1, "login failed"), ("cp", 2, "copy failed (deadline)")):
            self.log.unlink(missing_ok=True)
            self.install_tool(sleep=kind)
            def bounded(*args, **kwargs):
                self.assertEqual(set(kwargs["env"]), {"HOME", "TMPDIR", "LANG", "LC_ALL"})
                self.assertGreater(kwargs["timeout"], 0)
                self.assertLessEqual(kwargs["timeout"], 600)
                return run(*args, **dict(kwargs, timeout=1))
            with mock.patch.object(self.api.subprocess, "run", side_effect=bounded):
                self.refused(calls=count, diagnostic=diagnostic)


if __name__ == "__main__":
    unittest.main()
