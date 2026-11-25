"""Local fake-tool contracts only; not real ORAS/BuildKit/transport approval.
Run: python3 -m unittest discover -s tests -p oci_stage_test.py -v
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import re
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
TRANSIENT = {"registry-server", "registry-rate-limit", "registry-readback", "transport"}
MARKERS = "|".join(['unexpected-eof', 'eof', 'connection-reset', 'broken-pipe', 'io-timeout', 'tls-handshake-timeout', 'context-deadline', 'client-timeout', 'no-such-host', 'connection-refused', 'http2-goaway', 'closed-connection'])
# The fixed shape of a failed copy's last line (#711): the category, the attempt, and
# signals made only of fixed words and numbers.
SIGNALS = (r"status (?:none|[1-5][0-9]{2}); oci-code [a-z-]+; request (?:none|[A-Z]+ [a-z-]+); "
           r"host (?:none|ghcr\.io|other); transport (?:none|(?:" + MARKERS + r")(?:,(?:" + MARKERS + r"))*); stderr [0-9]+ lines(?: \(first 4096 bytes\))?"
           # The first line's skeleton: reviewed words, statuses, punctuation and placeholders only.
           r'; first-line "[a-z0-9\' :,<>\-…]{1,160}"')


def copy_failed(category, attempt):
    return rf"oci-stage: copy failed \({re.escape(category)}\) \[attempt {attempt} of 3; {SIGNALS}\]"


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
        self.sleeps = []
        sleeper = mock.patch.object(self.api, "wait", side_effect=self.sleeps.append)
        sleeper.start()
        self.addCleanup(sleeper.stop)
        self.graph = fixture()
        self.archive = self.base / "archive.tar"
        self.write_archive()
        self.env = dict(os.environ, GITHUB_TOKEN=TOKEN, GITHUB_ACTOR="fixture-actor",
                        HTTPS_PROXY="https://credential.invalid", http_proxy="proxy-secret",
                        ALL_PROXY="proxy-secret", NO_PROXY="ambient", DOCKER_CONFIG="helper-secret",
                        DOCKER_AUTH_CONFIG="auth-secret", GH_TOKEN="gh-secret",
                        ACTIONS_ID_TOKEN_REQUEST_TOKEN="oidc-secret", PATH="/ambient/helpers",
                        HOME=str(self.base / "ambient-home"), TMPDIR=str(self.base / "ambient-tmp"))

    def install_tool(self, fail="", sleep="", diagnostic=None, cp_failures=None):
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
failures = {cp_failures!r}
if kind == 'cp' and failures is not None:
    seen = sum(1 for line in open({str(self.log)!r}) if json.loads(line)['argv'][0] == 'cp')
    if seen <= failures:
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
        elif diagnostic.startswith("copy failed ("):
            category = diagnostic[len("copy failed ("):-1]
            attempts = calls - 1
            lines = err.splitlines()
            self.assertEqual(len(lines), attempts, err)
            for attempt, line in enumerate(lines[:-1], 1):
                self.assertRegex(line, rf"^oci-stage: copy attempt {attempt} of 3 failed \({re.escape(category)}\) \[{SIGNALS}\]; retrying in [0-9]+ s$")
            self.assertRegex(lines[-1], f"^{copy_failed(category, attempts)}$")
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
        self.assertEqual((result.returncode, result.stdout), (1, b""))
        self.assertRegex(result.stderr.decode(), f"^{copy_failed('registry-access', 1)}\n$")
        self.assertIn("oci-code denied; request none; host none", result.stderr.decode())
        self.assertNotIn(b"fixture-private-token", result.stderr)
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
            (destination + b"Head " + url + b": tls: failed to verify certificate: x509: unknown authority", "transport-tls"),
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
                # A transient failure is retried to the bound; any other is not.
                self.refused(calls=1 + (3 if category in TRANSIENT else 1), diagnostic=f"copy failed ({category})")

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

    def test_warning_free_report_byte_limit_includes_trailing_lines(self):
        line = b"Error response from registry: denied\n"
        exact = line + b"x" * (4095 - len(line)) + b"\n"
        for report, label in ((exact, "registry-access"), (exact + b"\n", "unknown")):
            self.assertEqual(self.api.copy_failure_category(io.BytesIO(report), IMAGE), label)

    def test_copy_process_outcome_precedes_stderr(self):
        for code, label in ((1, "registry-access"), (9, "unknown"), (-15, "signal")):
            def terminated(args, **kwargs):
                kwargs["stderr"].write(b"Error response from registry: denied\n")
                return subprocess.CompletedProcess(args, code)
            with mock.patch.object(self.api.subprocess, "run", side_effect=terminated):
                self.assertEqual(self.api.attempt_copy(self.tool, [], {}, self.base, IMAGE, 60)[0], label)
        with mock.patch.object(self.api.subprocess, "run", side_effect=FileNotFoundError()):
            self.assertEqual(self.api.attempt_copy(self.tool, [], {}, self.base, IMAGE, 60)[0], "execution")

    # The error shapes ORAS 1.3.6 (the pinned build) printed for a copy to a local stand-in
    # registry that failed in each way, with ghcr.io and the image in place of the stand-in.
    DESTINATION = f'Error from destination registry for "{IMAGE}": '.encode()
    ORAS_OUTPUT = {
        "503 without a body": (DESTINATION + b'POST "https://ghcr.io/v2/bitop-dev/open-model-gateway/blobs/uploads/": response status code 503: Service Unavailable',
                               "registry-server", "status 503; oci-code none; request POST blob-upload; host ghcr.io; transport none"),
        "500 with an UNKNOWN error body": (DESTINATION + b"unknown: internal", "unknown", "status none; oci-code unknown; request none; host none; transport none"),
        "429 with an error body": (DESTINATION + b"toomanyrequests: retry later", "registry-rate-limit", "status none; oci-code toomanyrequests; request none; host none; transport none"),
        "401": (DESTINATION + b"unauthorized: authentication required", "registry-access", "status none; oci-code unauthorized; request none; host none; transport none"),
        "403": (DESTINATION + b"denied: permission_denied", "registry-access", "status none; oci-code denied; request none; host none; transport none"),
        "a closed connection": (DESTINATION + b'Post "https://ghcr.io/v2/bitop-dev/open-model-gateway/blobs/uploads/?secret=fixture-private-token": EOF',
                                "transport", "status none; oci-code none; request POST blob-upload; host ghcr.io; transport eof"),
    }

    def test_copy_signals_name_status_code_request_and_host_in_fixed_words_only(self):
        for label, (stderr, category, signals) in self.ORAS_OUTPUT.items():
            with self.subTest(label=label):
                report = io.BytesIO(stderr + b"\n")
                self.assertEqual(self.api.copy_failure_category(report, IMAGE), category)
                self.assertEqual(self.api.copy_signals(report), signals + "; stderr 1 lines")
        noisy = io.BytesIO(b"x" * 5000 + b'PUT "https://evil.example/v2/a/manifests/b?t=fixture-private-token": i/o timeout\n')
        signals = self.api.copy_signals(noisy)
        self.assertTrue(signals.endswith("(first 4096 bytes)"), signals)
        self.assertNotIn(TOKEN, signals)
        self.assertNotIn("evil", signals)

    def test_a_transient_copy_failure_is_retried_and_then_succeeds(self):
        stderr, _, _ = self.ORAS_OUTPUT["503 without a body"]
        for failures, sleeps in ((1, [10]), (2, [10, 30])):
            with self.subTest(failures=failures):
                self.log.unlink(missing_ok=True)
                self.sleeps.clear()
                self.install_tool(diagnostic=stderr + b"\n", cp_failures=failures)
                code, out, err = self.invoke()
                self.assertEqual(code, 0, err)
                self.assertEqual(out, self.graph["root"]["digest"] + "\n")
                self.assertEqual(self.sleeps, sleeps)
                lines = err.splitlines()
                self.assertEqual(len(lines), failures)
                for attempt, (line, pause) in enumerate(zip(lines, (10, 30)), 1):
                    self.assertRegex(line, rf"^oci-stage: copy attempt {attempt} of 3 failed \(registry-server\) \[{SIGNALS}\]; retrying in {pause} s$")
                copies = [r for r in self.records() if r["argv"][0] == "cp"]
                self.assertEqual(len(copies), failures + 1)
                # Every attempt copies the same verified archive by the same root digest.
                self.assertEqual({json.dumps(r["argv"]) for r in copies}, {json.dumps(copies[0]["argv"])})
                self.assertEqual({r["archive_sha256"] for r in copies}, {copies[0]["archive_sha256"]})
                self.assertEqual(sum(1 for r in self.records() if r["argv"][0] == "login"), 1)
                self.assert_cleanup()

    def test_retries_end_at_the_attempt_bound_and_within_the_time_budget(self):
        stderr, _, _ = self.ORAS_OUTPUT["a closed connection"]
        self.install_tool(fail="cp", diagnostic=stderr + b"\n")
        self.refused(calls=4, diagnostic="copy failed (transport)")
        self.assertEqual(self.sleeps, [10, 30])
        # With the budget nearly spent, the next pause would not fit: no retry.
        self.log.unlink(missing_ok=True)
        self.sleeps.clear()
        clock = iter([0.0, 0.0, 595.0, 595.0, 595.0])
        with mock.patch.object(self.api.time, "monotonic", side_effect=lambda: next(clock)):
            self.refused(calls=2, diagnostic="copy failed (transport)")
        self.assertEqual(self.sleeps, [])

    def test_each_attempt_gets_the_rest_of_one_budget(self):
        stderr, _, _ = self.ORAS_OUTPUT["503 without a body"]
        self.install_tool(fail="cp", diagnostic=stderr + b"\n")
        # A clock that only the pauses move: attempt n gets 600 s minus the pauses before it.
        now = [1000.0]
        def pause(seconds):
            self.sleeps.append(seconds)
            now[0] += seconds
        timeouts, run = [], subprocess.run
        def recorded(*args, **kwargs):
            if args[0][1] == "cp":
                timeouts.append(kwargs["timeout"])
            return run(*args, **kwargs)
        with mock.patch.object(self.api, "wait", side_effect=pause), \
                mock.patch.object(self.api.time, "monotonic", side_effect=lambda: now[0]), \
                mock.patch.object(self.api.subprocess, "run", side_effect=recorded):
            self.refused(calls=4, diagnostic="copy failed (registry-server)")
        self.assertEqual(timeouts, [600, 590, 560])
        self.assertEqual(self.sleeps, [10, 30])

    def test_a_retry_needs_a_minute_of_budget_after_its_pause(self):
        stderr, _, _ = self.ORAS_OUTPUT["503 without a body"]
        for left, retried in ((69.0, False), (70.0, True)):
            with self.subTest(left=left):
                self.log.unlink(missing_ok=True)
                self.sleeps.clear()
                self.install_tool(diagnostic=stderr + b"\n", cp_failures=1)
                clock = iter([0.0, 0.0] + [600.0 - left] * 10)
                with mock.patch.object(self.api.time, "monotonic", side_effect=lambda: next(clock)):
                    code, _, _ = self.invoke()
                self.assertEqual(code == 0, retried)
                self.assertEqual(self.sleeps, [10] if retried else [])

    # ORAS re-reads each manifest it pushed (its text status output lists the
    # manifest's children) and a 404 there reads exactly so (#711), naming a
    # manifest of the archive.
    @property
    def READBACK(self):
        return b"Error response from registry: " + self.graph["root"]["digest"].encode() + b": not found"

    def test_a_manifest_the_registry_does_not_serve_yet_is_retried_and_then_succeeds(self):
        root = frozenset([self.graph["root"]["digest"]])
        self.assertEqual(self.api.copy_failure_category(io.BytesIO(self.READBACK + b"\n"), IMAGE, root), "registry-readback")
        # After a registry warning, the read-back as the last line is still the form.
        warning = b"[2026-11-25T20:03:00Z][WARNING]: read\n[Data]:\n  registry=ghcr.io\n\n\n"
        self.assertEqual(self.api.copy_failure_category(io.BytesIO(warning + self.READBACK + b"\n"), IMAGE, root),
                         "registry-readback")
        self.install_tool(diagnostic=self.READBACK + b"\n", cp_failures=1)
        code, out, err = self.invoke()
        self.assertEqual((code, out), (0, self.graph["root"]["digest"] + "\n"), err)
        self.assertEqual(self.sleeps, [10])
        self.assertEqual(err, (
            "oci-stage: copy attempt 1 of 3 failed (registry-readback) [status none; oci-code none; request none; "
            "host none; transport none; stderr 1 lines; first-line \"error response from registry: <x>: <x>: not found\"]; "
            "retrying in 10 s\n"))
        copies = [r for r in self.records() if r["argv"][0] == "cp"]
        self.assertEqual(len(copies), 2)
        self.assertEqual({json.dumps(r["argv"]) for r in copies}, {json.dumps(copies[0]["argv"])})

    def test_a_manifest_never_served_ends_in_a_refusal_naming_the_form(self):
        self.install_tool(fail="cp", diagnostic=self.READBACK + b"\n")
        self.refused(calls=4, diagnostic="copy failed (registry-readback)")
        self.assertEqual(self.sleeps, [10, 30])
        _, _, err = self.invoke()
        self.assertIn('first-line "error response from registry: <x>: <x>: not found"]', err.splitlines()[-1])

    def test_only_the_exact_readback_form_is_retried(self):
        digest = self.graph["root"]["digest"].encode()
        for line in (
            # A digest that is not a manifest of the archive: not this copy's read-back.
            b"Error response from registry: sha256:" + b"7" * 64 + b": not found",
            # Another prefix: that is a copy error of the destination, not the read-back.
            self.DESTINATION + digest + b": not found",
            # Anything before, after or instead of the digest is not ORAS's form.
            b"Error response from registry: unauthorized: " + digest + b": not found",
            b"Error response from registry: " + digest + b": not found; denied: forbidden",
            b"Error response from registry: " + digest + b": not found: retry",
            b"Error response from registry: sha256:" + b"7" * 63 + b": not found",
            b"Error response from registry: sha256:" + b"A" * 64 + b": not found",
            b"Error response from registry: sha512:" + b"7" * 128 + b": not found",
            b"Error response from registry: " + digest + b": manifest unknown",
            b"Error response from registry:  " + digest + b": not found",
            # Real terminal errors, also as bare codes and joined with a rate limit.
            b"Error response from registry: unauthorized: authentication required",
            b"Error response from registry: denied: permission_denied",
            self.DESTINATION + b"unauthorized: authentication required",
            self.DESTINATION + b"toomanyrequests: slow; denied",
            self.DESTINATION + b"toomanyrequests: slow; unauthorized; denied: no",
            b"Error response from registry: toomanyrequests; denied",
        ):
            with self.subTest(line=line):
                self.log.unlink(missing_ok=True)
                self.sleeps.clear()
                category = self.api.copy_failure_category(io.BytesIO(line + b"\n"), IMAGE, frozenset([digest.decode()]))
                self.assertFalse(category in TRANSIENT and not self.api.terminal(io.BytesIO(line + b"\n")), category)
                self.install_tool(fail="cp", diagnostic=line + b"\n")
                code, _, err = self.invoke()
                self.assertEqual(code, 1)
                self.assertEqual(sum(1 for r in self.records() if r["argv"][0] == "cp"), 1, err)
                self.assertEqual(self.sleeps, [])
        # The form must be the whole report: after a warning, or with a second line, it is not.
        warning = (b"[2026-11-25T20:03:00Z][WARNING]: read\n[Data]:\n  registry=ghcr.io\n\n\n")
        for report in (warning + self.READBACK + b"\nmore\n", self.READBACK + b"\nmore\n", self.READBACK):
            self.assertEqual(self.api.copy_failure_category(io.BytesIO(report), IMAGE, frozenset([digest.decode()])), "unknown")

    def test_signals_never_come_from_registry_text(self):
        # Registry-chosen text that imitates a request, a status, a code or a
        # transport error is not read as one: fields come only from where ORAS
        # writes them in its error line.
        none = "status none; oci-code none; request none; host none; transport none; stderr {} lines"
        for stderr, signals in (
            (self.DESTINATION + b'unknown: GET "https://example.invalid/v2/test/manifests/example": response status code 503: read',
             "status none; oci-code unknown; request none; host none; transport none; stderr 1 lines"),
            (self.DESTINATION + b'POST "https://ghcr.io/v2/a/blobs/uploads/": response status code 400: i/o timeout; EOF',
             "status 400; oci-code none; request POST blob-upload; host ghcr.io; transport none; stderr 1 lines"),
            (b"[2026-11-25T20:03:00Z][WARNING]: GET \"https://x.invalid/v2/a/manifests/b\": response status code 503: EOF\n"
             b"[Data]:\n  registry=ghcr.io\n\n\n" + self.DESTINATION + b"denied: no",
             "status none; oci-code denied; request none; host none; transport none; stderr 6 lines"),
            (b'note: GET "https://ghcr.io/v2/a/manifests/b": response status code 503: EOF', none.format(1)),
            (b'toomanyrequests: x\n' + self.DESTINATION + b"denied: no", none.format(2)),
            # oras-go's own response checks after a request record carry header
            # values, not a transport error.
            (self.DESTINATION + b'GET "https://ghcr.io/v2/a/manifests/b": invalid response Content-Type: EOF i/o timeout',
             "status none; oci-code none; request GET manifest; host ghcr.io; transport none; stderr 1 lines"),
            # A bare code (no message) is the code; joined entries name the first.
            (self.DESTINATION + b"denied", "status none; oci-code denied; request none; host none; transport none; stderr 1 lines"),
            (self.DESTINATION + b"toomanyrequests; denied: x",
             "status none; oci-code toomanyrequests; request none; host none; transport none; stderr 1 lines"),
        ):
            with self.subTest(signals=signals):
                self.assertEqual(self.api.copy_signals(io.BytesIO(stderr + b"\n")), signals)

    def test_a_rate_limit_reported_with_a_denial_or_content_error_is_not_retried(self):
        for detail in (b"toomanyrequests: slow; denied: forbidden", b"toomanyrequests: slow; digest invalid: wrong bytes",
                       b'POST "https://ghcr.io/v2/a/blobs/uploads/": response status code 429: x; unauthorized: no'):
            for prefix in (self.DESTINATION, b"Error response from registry: "):
                with self.subTest(detail=detail, prefix=prefix):
                    self.log.unlink(missing_ok=True)
                    self.sleeps.clear()
                    self.install_tool(fail="cp", diagnostic=prefix + detail + b"\n")
                    code, _, err = self.invoke()
                    self.assertEqual(code, 1)
                    self.assertEqual(sum(1 for r in self.records() if r["argv"][0] == "cp"), 1, err)
                    self.assertEqual(self.sleeps, [])

    def test_the_first_line_skeleton_keeps_only_reviewed_words(self):
        def sk(stderr):
            return self.api.skeleton(io.BytesIO(stderr))
        line = lambda text: text + b"\nsecond line " + TOKEN.encode() + b"\n"
        cases = (
            # A path is a placeholder; the error words stay readable.
            (b"Error: failed to open /home/runner/work/x/image.tar: no such file or directory",
             "error: failed to open <x>: no such file or directory"),
            # A bearer token and a key=value secret never survive.
            (b"Error: Bearer " + TOKEN.encode() + b" rejected; token=" + TOKEN.encode(), "error: bearer <x>"),
            # A quoted segment (a reference, URL or value) is one placeholder,
            # even when it holds listed words or a query with a colon.
            (b'Error: GET "https://ghcr.io/token?scope=repository%3Aa%3Apush&access_token=' + TOKEN.encode() + b'": unexpected EOF',
             "error: get <x>: unexpected eof"),
            (b'Error: password "COPY"; host "cache"; file "config"; repository "backup"',
             "error: <x> host <x> file <x> repository <x>"),
            (b'Error: "https://registry.fixture.invalid/v2?fixture=opaque:copy"', "error: <x>"),
            (b'Error: failed to resolve "sha256:2f1c" in OCI layout: not found',
             "error: failed to resolve <x> in oci layout: not found"),
            # Letter case carries nothing; words outside the list are placeholders.
            (b"ERROR: Failed To Open hunter2Secret", "unrecognised: error: failed to open <x>"),
            (b"Error: FAILED To Open hunter2Secret", "error: failed to open <x>"),
            # A number survives only as the status of a status code.
            (b"Error: response status code 503: 5000 12 0x1f", "error: response status code 503: <x>"),
            (b"Error: pin 123, 599 600", "error: <x>, <x>"),
            # Control characters and non-ASCII text are placeholders; CR ends the line.
            ("Error: \x1b[31mrouge élève\x07".encode(), "error: <x>"),
            (b"Error: read\rpassword copy", "error: read"),
            # Brackets, quotes and semicolons never show: the skeleton cannot
            # imitate the fields printed around it.
            (b'Error: copy"]; status 418; [read', "error: <x>"),
            # Every first line is shown under the name of its form (below).
            (b"[2026-11-22T08:12:08.123Z][WARNING]: copy read", "warning: <x> read"),
            (b"\nError: failed", "blank"),
            (b"", "blank"),
        )
        for text, expected in cases:
            with self.subTest(expected=expected):
                got = sk(line(text))
                self.assertEqual(got, expected)
                self.assertNotIn(TOKEN, got)
        self.assertEqual(sk(b""), "none")
        # A first line longer than the report is still named, its cut token
        # dropped (a cut token may read as a listed word) and the cut marked.
        # (4096 bytes end inside "opened", at "open".)
        for long in (b"Error: failed " + b"x" * 5000, b"Error: " + b"x" * 4084 + b" opened" + b"x" * 100):
            got = sk(long + b"\n")
            self.assertTrue(got.startswith("error: ") and got.endswith(" \u2026"), got)
            self.assertNotIn("open", got)
        # Bounded in tokens and characters, the omission marker included.
        for text in (b"Error: " + b"error " * 200, b"Error: " + b" ".join([b"failed"] * 30 + [b"x" * 50] * 30),
                     b"Error: " + b":" * 100):
            got = sk(text + b"\n")
            self.assertLessEqual(len(got), self.api.SKELETON_CHARS, got)
            self.assertTrue(got.endswith(" \u2026"), got)
        # A failed attempt reports it with the other signals, never the line itself.
        failure = b'Error: failed to push "ghcr.io/bitop-dev/x@sha256:aa": ' + TOKEN.encode()
        def failed(args, **kwargs):
            kwargs["stderr"].write(failure + b"\n")
            return subprocess.CompletedProcess(args, 1)
        with mock.patch.object(self.api.subprocess, "run", side_effect=failed):
            outcome = self.api.attempt_copy(self.tool, [], {}, self.base, IMAGE, 60)
        self.assertIn('first-line "error: failed to push <x>: <x>"', outcome[1])
        self.assertNotIn(TOKEN, outcome[1])
        self.assertNotIn("bitop", outcome[1])

    # Every form ORAS v1.3.6 (with cobra v1.10.2 and oras-go v2.7.0) or the Go
    # runtime prints as the first line of a failed `cp`, with its fixed name.
    FORMS = (
        (b"Error: ", "error:"),
        (b"Error response from registry: ", "error response from registry:"),
        (b'Error from source registry for "ghcr.io/bitop-dev/x:v1": ', "error from source registry for <x>:"),
        (b'Error from source oci-layout for "/tmp/oci-stage-x/image.tar@sha256:aa": ',
         "error from source oci-layout for <x>:"),
        (b'Error from destination registry for "ghcr.io/bitop-dev/open-model-gateway": ',
         "error from destination registry for <x>:"),
        (b'Error from destination oci-layout for "dir:v1": ', "error from destination oci-layout for <x>:"),
        (b"[2026-11-25T16:33:08.123456789Z][WARNING]: ", "warning:"),
        (b"[2026-11-25T16:33:08+01:00][WARNING]: ", "warning:"),
        (b"panic: ", "panic:"),
        (b"fatal error: ", "fatal error:"),
        (b"runtime: ", "runtime:"),
        (b"unexpected fault address ", "unexpected fault address"),
    )

    def test_every_first_line_form_is_named_and_masked_as_before(self):
        def sk(stderr):
            return self.api.skeleton(io.BytesIO(stderr))
        for prefix, name in self.FORMS:
            with self.subTest(form=name):
                self.assertEqual(sk(prefix + b"failed to open /tmp/x: no such file or directory\n"),
                                 name + " failed to open <x>: no such file or directory")
                self.assertEqual(sk(prefix + b"\n"), name)
        # The reference ORAS quotes (Go %q) is one placeholder, escapes included.
        self.assertEqual(sk(b'Error from destination registry for "a\\" copy read": denied\n'),
                         "error from destination registry for <x>: denied")
        # A prefix that is not exactly one of the forms is shown as unrecognised.
        for line, expected in (
            (b"Error from destination copy for \"x\": denied", "unrecognised: error from destination <x> for <x>: denied"),
            (b"Error response from registry:denied", "unrecognised: error response from registry: denied"),
            (b"[2026-11-25][WARNING]: read", "unrecognised: <x>: read"),
            (b"error: failed", "unrecognised: error: failed"),
            (b"  Error: failed", "unrecognised: error: failed"),
            (b"Run 'oras cp --help' for usage.", "unrecognised: <x> for <x>"),
            (b"   ", "blank"),
        ):
            with self.subTest(line=line):
                self.assertEqual(sk(line + b"\n"), expected)

    # The leak fixtures of the `Error: ` form, under every other form too.
    LEAKS = (
        b"Bearer " + TOKEN.encode() + b" rejected; token=" + TOKEN.encode(),
        b'GET "https://ghcr.io/token?scope=repository%3Abitop-dev%3Apush&access_token=' + TOKEN.encode() + b'": unexpected EOF',
        b'password "COPY"; host "cache"; file "config"; repository "bitop-dev/backup"',
        b'"https://registry.fixture.invalid/v2?fixture=opaque:copy"',
        b"Failed To Open hunter2Secret " + TOKEN.encode(),
        "\x1b[31mrouge élève\x07 ".encode() + TOKEN.encode(),
        b"read\r" + TOKEN.encode() + b" copy",
        b'copy"]; status 418; [read ' + TOKEN.encode(),
        b"unauthorized: " + TOKEN.encode() + b": " + b"x" * 4000,
        b'"' + TOKEN.encode() + b'\\" ' + TOKEN.encode() + b' "',
    )

    def test_no_form_lets_a_secret_or_a_value_through(self):
        allowed = re.compile(r"[a-z0-9' :,<>\-…]{1,160}")
        words = self.api.SKELETON_TOKENS + 1  # the omission marker
        for prefix, name in self.FORMS + ((b"", "unrecognised:"), (b"Error from destination copy for ", "unrecognised:")):
            for leak in self.LEAKS:
                with self.subTest(form=name, leak=leak[:24]):
                    got = self.api.skeleton(io.BytesIO(prefix + leak + b"\nsecond " + TOKEN.encode() + b"\n"))
                    self.assertTrue(got.startswith(name), got)
                    self.assertIsNotNone(allowed.fullmatch(got), got)
                    self.assertLessEqual(len(got.split(" ")), words, got)
                    for secret in (TOKEN, "bitop", "hunter2", "fixture", "opaque", "cache", "418", "rouge"):
                        self.assertNotIn(secret, got)

    def test_quotes_cuts_and_bounds_hold_under_every_form(self):
        def sk(stderr):
            return self.api.skeleton(io.BytesIO(stderr))
        # A single-quoted segment is one placeholder; an apostrophe inside a
        # word keeps the word.
        self.assertEqual(sk(b"Error response from registry: denied: 'private read denied value'\n"),
                         "error response from registry: denied: <x>")
        self.assertEqual(sk(b"Error: it doesn't exist 'unterminated read\n"), "error: it doesn't exist <x>")
        # What masking cannot do: a value that is itself a listed word shows,
        # on every form. Secrets here (tokens, paths, digests) never are.
        self.assertEqual(sk(b"Authorization: Bearer read\n"), "unrecognised: <x>: bearer read")
        # A line longer than the report: the token the cut ends is a
        # placeholder, whatever separates it, and the cut is marked.
        for line, expected in (
            (b"x" * 4097, "unrecognised: <x> \u2026"),
            (b"Error: " + b"x" * 4083 + b"\topened", "error: <x> \u2026"),
            (b"Error: " + b"x" * 4084 + b" opened" + b"x" * 100, "error: <x> \u2026"),
        ):
            with self.subTest(line=line[-12:]):
                self.assertEqual(sk(line + b"\n"), expected)
        # A line of exactly the report's size, ended by its newline, is whole.
        whole = b"Error: " + b"x" * (4096 - 7 - 7) + b"\tdenied"
        self.assertEqual(len(whole), 4096)
        for end in (b"\n", b"\r"):
            self.assertEqual(sk(whole + end + b"more\n"), "error: <x> denied")
        # The name's words count against the bound.
        got = sk(b'Error from destination registry for "repo": ' + b"a " * 60 + b"\n")
        self.assertLessEqual(len(got.split(" ")), self.api.SKELETON_TOKENS + 1, got)
        self.assertTrue(got.endswith(" \u2026"), got)

    def test_the_line_of_the_first_unknown_failure_is_named(self):
        # #711: one stderr line that was not ORAS's `Error: ` form, reported as
        # `first-line "none"`. The same report now names the form.
        line = (b'Error from destination registry for "ghcr.io/bitop-dev/open-model-gateway": '
                b"service unavailable: " + TOKEN.encode())
        self.install_tool(fail="cp", diagnostic=line + b"\n")
        code, out, err = self.invoke()
        self.assertEqual((code, out), (1, ""))
        self.assertEqual(err.strip(), (
            "oci-stage: copy failed (unknown) [attempt 1 of 3; status none; oci-code none; request none; "
            'host none; transport none; stderr 1 lines; first-line "error from destination registry for <x>: <x>: <x>"]'))
        self.assertEqual(sum(1 for r in self.records() if r["argv"][0] == "cp"), 1)

    def test_signals_come_from_request_records_only(self):
        cases = (
            (self.DESTINATION + b"denied: xresponse status code 503-opaque", "status none"),
            (self.DESTINATION + b'GET "https://ghcr.io/v2/a/blobs/sha256:1?next=/manifests/x": EOF', "request GET blob"),
            (self.DESTINATION + b'GET "https://ghcr.io/v2/?x=/token": EOF', "request GET registry-root"),
        )
        for stderr, expected in cases:
            with self.subTest(expected=expected):
                self.assertIn(expected, self.api.copy_signals(io.BytesIO(stderr + b"\n")))
        # A report over 4096 bytes is read only to its last whole line.
        long = b"y" * 4090 + b"\n" + self.DESTINATION + b'PUT "https://ghcr.io/v2/a/manifests/b": EOF\n'
        signals = self.api.copy_signals(io.BytesIO(long))
        self.assertIn("request none", signals)
        self.assertTrue(signals.endswith("stderr 1 lines (first 4096 bytes)"), signals)

    def test_a_timed_out_attempt_reports_what_it_wrote(self):
        def slow(args, **kwargs):
            kwargs["stderr"].write(self.DESTINATION + b'PUT "https://ghcr.io/v2/a/blobs/uploads/x": EOF\n')
            raise subprocess.TimeoutExpired(args, kwargs["timeout"])
        with mock.patch.object(self.api.subprocess, "run", side_effect=slow):
            category, signals, retryable = self.api.attempt_copy(self.tool, [], {}, self.base, IMAGE, 60)
        self.assertEqual((category, retryable), ("deadline", False))
        self.assertIn("request PUT blob-upload; host ghcr.io; transport eof; stderr 1 lines", signals)

    def test_no_attempt_starts_with_less_than_a_minute_left_after_a_long_pause(self):
        stderr, _, _ = self.ORAS_OUTPUT["503 without a body"]
        self.install_tool(diagnostic=stderr + b"\n", cp_failures=1)
        # 500 s left before the pause; after it overruns, 45 s left: no second attempt.
        clock = iter([0.0, 0.0, 100.0, 555.0, 555.0, 555.0])
        with mock.patch.object(self.api.time, "monotonic", side_effect=lambda: next(clock)):
            code, out, err = self.invoke()
        self.assertEqual((code, out), (1, ""))
        announced, final = err.splitlines()
        self.assertTrue(announced.endswith("; retrying in 10 s"), announced)
        self.assertRegex(final, f"^{copy_failed('registry-server', 1)}$")
        self.assertEqual(sum(1 for r in self.records() if r["argv"][0] == "cp"), 1)
        self.assertEqual(self.sleeps, [10])

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
