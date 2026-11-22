#!/usr/bin/env python3
"""Private digest-only OCI importer; no root approval or producer authentication.

The external workflow must select trusted control/tool artifacts and approved
roots. Synthetic tests do not establish real BuildKit/ORAS compatibility. No
index assembly, tags, signing, image execution, or fallback transport lives here.
Missing reviewed .github/tools/oras.json intentionally prevents execution.
"""
import argparse
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile

CONTROL = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("oci_layout", CONTROL / "oci-layout.py")
layout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layout)
IMAGES = {"ghcr.io/bitop-dev/open-model-gateway", "ghcr.io/bitop-dev/open-model-gateway-loadtest"}
SOURCE_COMMIT = "9b1fc815f6b7fba743f08b9b5c5846639cc11d3c"
DIAGNOSTIC_LINE_BYTES = 4096
REGISTRY_CODES = {
    b"unauthorized": "registry-access",
    b"denied": "registry-access",
    b"toomanyrequests": "registry-rate-limit",
    b"blob unknown": "registry-content",
    b"manifest blob unknown": "registry-content",
    b"manifest invalid": "registry-content",
    b"digest invalid": "registry-content",
    b"size invalid": "registry-content",
}
GHCR_REQUEST = re.compile(
    rb'(?:GET|HEAD|POST|PATCH|PUT|DELETE) "https://ghcr\.io/[^"\x00-\x20\x7f]*": ',
    re.IGNORECASE,
)


class Refusal(Exception):
    pass


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise ValueError("invalid arguments")


@contextmanager
def regular(path, executable=False):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        mode = os.fstat(stream.fileno()).st_mode
        if not stat.S_ISREG(mode) or (executable and not mode & 0o111):
            raise ValueError("not a regular executable/file")
        yield stream


def pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError("duplicate pin field")
        result[key] = value
    return result


def pin():
    tools = CONTROL.parent / "tools"
    if tools.is_symlink():
        raise ValueError("symlink tools directory")
    with regular(tools / "oras.json") as stream:
        data = stream.read(4097)
    if len(data) > 4096:
        raise ValueError("pin size limit")
    config = json.loads(data, object_pairs_hook=pairs)
    if (type(config) is not dict or set(config) != {"version", "source_commit", "binary_sha256"}
            or config["version"] != "1.3.6" or config["source_commit"] != SOURCE_COMMIT
            or type(config["binary_sha256"]) is not str
            or not re.fullmatch(r"[0-9a-f]{64}", config["binary_sha256"])):
        raise ValueError("invalid tool pin")
    return config["binary_sha256"]


def snapshot(source, target, limit, executable=False):
    """Only private, bounded copies are subsequently inspected/executed/read."""
    hasher, total = hashlib.sha256(), 0
    with regular(source, executable) as src, target.open("xb") as dst:
        os.chmod(target, 0o600)
        while True:
            block = src.read(65536)
            if not block:
                break
            total += len(block)
            if total > limit:
                raise ValueError("snapshot byte limit")
            hasher.update(block)
            dst.write(block)
    return hasher.hexdigest()


def copy_failure_category(stderr, image):
    # The pinned ORAS CLI wraps registry errors with these prefixes. Its SDK
    # may render either an OCI error code or an HTTP status. Treat the rest as
    # untrusted and return only fixed labels from one bounded, complete line.
    stderr.seek(0)
    line = stderr.readline(DIAGNOSTIC_LINE_BYTES + 1)
    if len(line) > DIAGNOSTIC_LINE_BYTES or not line.endswith(b"\n"):
        return "unknown"
    line = line[:-1]
    for prefix in (b"Error response from registry: ",
                   f'Error from destination registry for "{image}": '.encode("ascii")):
        if line.startswith(prefix):
            detail = line[len(prefix):]
            break
    else:
        return "unknown"

    request = GHCR_REQUEST.match(detail)
    if request is None:
        return REGISTRY_CODES.get(detail.partition(b": ")[0], "unknown")
    detail = detail[request.end():]
    status = re.match(rb"response status code ([0-9]{3}): ", detail)
    if status:
        code = int(status[1])
        if code in (401, 403):
            return "registry-access"
        if code == 429:
            return "registry-rate-limit"
        if 500 <= code <= 599:
            return "registry-server"
    if (detail == b"net/http: TLS handshake timeout"
            or detail.startswith((b"dial tcp: ", b"dial tcp ", b"tls: failed to verify certificate: "))):
        return "transport"
    return "unknown"


def run(tool, args, env, work, reason, password=b"", *, copy_image=None):
    category = "unknown"
    try:
        # Discard stdout and keep stderr in a private temporary file so even
        # large child output never has to be buffered in memory.
        with tempfile.TemporaryFile(dir=work) as err:
            result = subprocess.run([str(tool), *args], input=password, env=env,
                                    cwd=work, stdout=subprocess.DEVNULL, stderr=err, timeout=600,
                                    check=False)
            if result.returncode == 0:
                return
            if copy_image is not None:
                category = copy_failure_category(err, copy_image)
    except subprocess.TimeoutExpired:
        category = "deadline"
    except (OSError, subprocess.SubprocessError):
        pass
    if copy_image is not None:
        reason += f" ({category})"
    raise Refusal(reason)


def stage(args):
    expected = pin()
    # Do not honor ambient TMPDIR, HOME, PATH, proxies, Docker helpers or tokens.
    with tempfile.TemporaryDirectory(prefix="oci-stage-", dir="/tmp") as directory:
        private = Path(directory)
        tool, archive = private / "oras", private / "image.tar"
        if snapshot(args.tool, tool, 256 * 1024**2, executable=True) != expected:
            raise ValueError("tool digest mismatch")
        tool.chmod(0o500)
        snapshot(args.archive, archive, layout.MAX_TOTAL_BYTES)
        archive.chmod(0o400)
        layout.inspect_archive(archive, args.root, args.platform)
        home, work = private / "home", private / "work"
        home.mkdir(mode=0o700)
        work.mkdir(mode=0o700)
        auth = home / "auth.json"
        with open(auth, "x", opener=lambda path, flags: os.open(path, flags, 0o600)) as stream:
            stream.write('{"auths":{}}')
        env = {"HOME": str(home), "TMPDIR": str(work), "LANG": "C", "LC_ALL": "C"}
        # This is the first credential access, after complete local validation.
        token, actor = os.environ.get("GITHUB_TOKEN", ""), os.environ.get("GITHUB_ACTOR", "")
        if (not token or any(c in token for c in "\r\n\x00")
                or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*(?:\[bot\])?", actor)):
            raise Refusal("authentication unavailable")
        run(tool, ["login", "--registry-config", str(auth), "--username", actor,
                   "--password-stdin", "ghcr.io"], env, work, "login failed", token.encode())
        # ORAS writes this file; retain private mode before its second use.
        auth.chmod(0o600)
        run(tool, ["cp", "--force", "--no-tty", "--from-oci-layout",
                   "--to-registry-config", str(auth), "--to-distribution-spec",
                   "v1.1-referrers-api", str(archive) + "@" + args.root, args.image],
            env, work, "copy failed", copy_image=args.image)


def main(argv=None):
    try:
        argv = sys.argv[1:] if argv is None else argv
        parser = Parser(add_help=False, allow_abbrev=False)
        fields = ("archive", "root", "platform", "image", "tool")
        for field in fields:
            parser.add_argument("--" + field, required=True)
        if len(argv) != 10 or set(argv[::2]) != {"--" + field for field in fields}:
            raise ValueError("invalid arguments")
        args = parser.parse_args(argv)
        if (args.image not in IMAGES or args.platform not in ("linux/amd64", "linux/arm64")
                or not re.fullmatch(r"sha256:[0-9a-f]{64}", args.root)):
            raise ValueError("invalid arguments")
        stage(args)
    except Refusal as exc:
        print("oci-stage: " + str(exc), file=sys.stderr)
        return 1
    except (OSError, ValueError, UnicodeError, RecursionError):
        print("oci-stage: validation failed", file=sys.stderr)
        return 1
    print(args.root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
