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
import time

CONTROL = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("oci_layout", CONTROL / "oci-layout.py")
layout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layout)
IMAGES = {"ghcr.io/bitop-dev/open-model-gateway", "ghcr.io/bitop-dev/open-model-gateway-loadtest"}
SOURCE_COMMIT = "9b1fc815f6b7fba743f08b9b5c5846639cc11d3c"
DIAGNOSTIC_BYTES = 4096
MAX_REGISTRY_WARNINGS = 3
# ORAS TextFormatter prints the timestamp, one message, registry field and
# separator. Only single-line warning text has an unambiguous boundary.
REGISTRY_WARNING = re.compile(
    rb"\[\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2})\]"
    rb"\[WARNING\]: [^\x00-\x1f\x7f]*\n\[Data\]:\n  registry=ghcr\.io\n\n\n"
)
REGISTRY_CODES = {
    b"unauthorized": "registry-access",
    b"denied": "registry-access",
    b"toomanyrequests": "registry-rate-limit",
    b"blob unknown": "registry-content",
    b"blob upload unknown": "registry-content",
    b"blob upload invalid": "registry-content",
    b"manifest unknown": "registry-content",
    b"manifest blob unknown": "registry-content",
    b"manifest invalid": "registry-content",
    b"digest invalid": "registry-content",
    b"size invalid": "registry-content",
}
# A copy is retried only after a failure the classifier above recognises as
# transient, within one time budget for every attempt (#711). The archive and its
# root digest are verified locally before the first attempt, and every attempt
# copies the same verified archive by digest.
COPY_ATTEMPTS = 3
COPY_BACKOFF = (10, 30)
COPY_BUDGET = 600
# A retry is made only if this much of the budget is left after its pause.
MIN_ATTEMPT = 60
# Login is one token exchange. With the copy budget it stays inside the import
# job's 15 minutes, which also covers checkout, downloads and validation.
LOGIN_TIMEOUT = 120
# A failure is retried only if nothing in its report says it is not transient:
# ORAS joins several OCI errors with "; " and may omit the HTTP status, so a
# rate limit can be reported together with a denial or a content error.
TERMINAL = re.compile(
    rb"(?:^|[:;] )(?:unauthorized|denied|blob unknown|blob upload invalid|blob upload unknown|"
    rb"digest invalid|manifest blob unknown|manifest invalid|manifest unknown|name invalid|"
    rb"name unknown|size invalid|unsupported): |failed to verify certificate|"
    rb'": response status code 4(?!29)[0-9]{2}\b',
    re.MULTILINE,
)
TRANSIENT = {"registry-server", "registry-rate-limit", "transport"}
# The diagnostic of a failed copy is built only from these fixed words, numbers
# and the match of a fixed alternation: never URL paths, queries, headers, tokens
# or registry message text.
OCI_CODE = re.compile(
    rb'(?:Error response from registry: |Error from destination registry for "[^"\n]*": )'
    rb"(blob unknown|blob upload invalid|blob upload unknown|digest invalid|manifest blob unknown|"
    rb"manifest invalid|manifest unknown|name invalid|name unknown|size invalid|unauthorized|denied|"
    rb"unsupported|toomanyrequests|unknown): "
)
REQUEST = re.compile(rb'\b(GET|HEAD|POST|PATCH|PUT|DELETE) "https?://([^/"\s?]+)(/[^"\s?]*)?(?:\?[^"\s]*)?"', re.IGNORECASE)
STATUS = re.compile(rb'"https?://[^"\s]+": response status code ([1-5][0-9]{2}): ')
TRANSPORT_MARKERS = (
    ("unexpected-eof", b"unexpected EOF"),
    ("eof", b": EOF"),
    ("connection-reset", b"connection reset by peer"),
    ("broken-pipe", b"broken pipe"),
    ("io-timeout", b"i/o timeout"),
    ("tls-handshake-timeout", b"TLS handshake timeout"),
    ("context-deadline", b"context deadline exceeded"),
    ("client-timeout", b"Client.Timeout exceeded"),
    ("no-such-host", b"no such host"),
    ("connection-refused", b"connection refused"),
    ("http2-goaway", b"GOAWAY"),
    ("closed-connection", b"use of closed network connection"),
)
# The skeleton of a failed copy's first stderr line (#711), under the name of
# its form (SKELETON_FORMS), or as unrecognised. A token survives only if it
# is one of these words
# (printed in lower case, so letter case carries nothing), or a three-digit
# HTTP status right after "status code"; every quoted segment (where ORAS
# puts references, paths and other values) and every other token (paths,
# hosts, digests, other numbers, tokens, words outside this list) becomes one
# placeholder, and a run of placeholders is one. Only ':' and ',' are kept,
# with fixed spacing: the input's spacing, brackets and quotes never show.
# The words are those of the error messages ORAS v1.3.6 (SOURCE_COMMIT) and
# oras-go v2.7.0 can print for `cp --from-oci-layout` (its command, options
# and internals, oras-go's errdef, content, oci, registry, remote and auth
# errors), with the words of the standard Go, go-digest and cobra errors
# they wrap (os and syscall, encoding/json, archive/tar, gzip, context, net,
# tls, x509, flags) and the OCI distribution error codes, less nouns that are
# also likely names or values (copy, cache, config, credential, token, ...).
# Never a value: this list only changes by review.
SKELETON_WORDS = frozenset((
    "a", "absolute", "accepts", "access", "actual", "add", "after", "algorithm", "allowed",
    "already", "an", "and", "annotation", "annotations", "another", "apply", "arch",
    "architecture", "archive", "are", "arg", "argument", "array", "arrays", "artifact",
    "artifact-type", "artifacts", "as", "at", "attempt", "auth", "authentication", "authority",
    "auths", "back", "bad", "base", "basic", "be", "bearer", "been", "before", "beginning",
    "between", "blob", "blocked", "body", "both", "broken", "build", "but", "by", "bytes",
    "calculate", "can", "canceled", "cannot", "capability", "certificate", "changed", "character",
    "characters", "check", "checksum", "children", "client", "close", "closed", "code", "colons",
    "command", "committed", "configured", "confirm", "conform", "conjunction", "connect",
    "connection", "contacted", "contain", "contains", "content", "content-length", "content-type",
    "context", "could", "count", "create", "current", "custom", "customized", "dangling", "data",
    "date", "deadline", "decode", "delete", "deletion", "denied", "depth", "descriptor",
    "destination", "determine", "device", "dial", "different", "digest", "digests", "dir",
    "directories", "directory", "disable-path-validation", "disabled", "disallowed", "display",
    "distribution", "does", "doesn't", "downgrades", "duplicate", "duplicated", "early", "either",
    "empty", "encoded", "end", "ensure", "eof", "error", "exceeded", "exceeds", "exist",
    "existence", "exists", "expect", "expected", "expecting", "extra", "extract", "fail", "failed",
    "fetch", "fetching", "field", "file", "files", "filter", "find", "flag", "flags", "for",
    "force", "format", "found", "from", "generate", "get", "getting", "given", "go", "got", "gzip",
    "handshake", "hard", "has", "have", "header", "helpers", "host", "http", "https", "identity",
    "if", "image", "image-spec", "implemented", "in", "index", "ingest", "initialized", "input",
    "insecure", "intentional", "into", "invalid", "is", "it", "it's", "json", "layer", "layout",
    "least", "left", "length", "like", "limit", "link", "list", "listing", "literal", "load",
    "location", "look", "looking", "lookup", "lstat", "made", "make", "manager", "manifest",
    "manifests", "many", "marshal", "match", "matching", "media", "media-type", "mismatch",
    "mismatched", "missing", "mkdir", "move", "multiple", "must", "name", "needs", "neither",
    "new", "next", "no", "nor", "not", "number", "object", "obtained", "oci", "of", "on", "once",
    "only", "open", "or", "oras", "os", "output", "outside", "over", "overwrite", "pages", "parse",
    "path", "paths", "peer", "permission", "pipe", "plaintext", "platform", "platforms", "pointer",
    "portion", "prefix", "prepare", "printing", "progress", "provided", "pulled", "push",
    "putting", "query", "read", "read-only", "readlink", "readonly", "received", "recognize",
    "recommended", "redirect", "redirects", "reference", "references", "referrer", "referrers",
    "refused", "refusing", "registry", "rejected", "reload", "remove", "rename", "repositories",
    "repository", "request", "requested", "required", "requires", "resend", "reserved", "reset",
    "resolve", "resolves", "resolving", "resource", "response", "restore", "returned",
    "rewindable", "running", "same", "save", "scheme", "seek", "seems", "set", "shorthand",
    "should", "signed", "size", "skip", "skipped", "source", "space", "specification", "specified",
    "stat", "status", "stdin", "stopped", "storage", "string", "sub-command", "subject",
    "successor", "such", "support", "supported", "symbolic", "symlink", "system", "tag", "tagged",
    "tags", "tar", "target", "tcp", "temporary", "text", "than", "the", "there", "this", "time",
    "timeout", "tls", "to", "too", "toomanyrequests", "top-level", "trailing", "traversal", "type",
    "udp", "unable", "unauthorized", "unexpected", "unknown", "unmarshal", "unmatched", "unnamed",
    "unsupported", "up", "update", "upload", "usage", "use", "used", "uses", "valid", "validate",
    "value", "values", "variant", "verify", "version", "via", "warning", "was", "when", "whence",
    "with", "write", "writing",
))
# The first-line forms of a failed `cp`, each by its fixed prefix: cobra
# v1.10.2 prints `<prefix> <error>`, where ORAS v1.3.6 sets the prefix to
# "Error response from registry:" for a registry's answer
# (cmd/oras/internal/errors) or to `Error from <origin> <type> for <%q
# reference>:` for an error of either end of a copy (option/binary_target.go),
# and cobra's own is "Error:". ORAS's log formatter starts a registry warning
# with `[<RFC 3339 time>][WARNING]: ` (internal/trace), and the Go runtime
# starts a crash with "panic: ", "fatal error: " or "runtime: ". The name
# printed for a form is fixed text; only the rest of the line is masked.
# After a registry's prefix the text is the registry's (its error code and
# message): it can choose among the listed words, never print a value.
SKELETON_FORMS = (
    (re.compile(rb"Error: "), "error:"),
    (re.compile(rb"Error response from registry: "), "error response from registry:"),
    (re.compile(rb'Error from (source|destination) (registry|oci-layout) for "(?:[^"\\]|\\.)*": '), None),
    (re.compile(rb"\[\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2})\]\[WARNING\]: "), "warning:"),
    (re.compile(rb"panic: "), "panic:"),
    (re.compile(rb"fatal error: "), "fatal error:"),
    (re.compile(rb"runtime: "), "runtime:"),
)
# A quoted segment runs to its closing quote, past Go's escaped quotes.
SKELETON_TOKEN = re.compile(rb'"(?:[^"\\]|\\.)*"?|[:,]|[^\s:,"]+')
SKELETON_WORD = re.compile(rb"[A-Za-z][A-Za-z'-]{0,23}")
SKELETON_STATUS = re.compile(rb"[1-5][0-9]{2}")
SKELETON_TOKENS = 40
# Including the omission marker.
SKELETON_CHARS = 160
SKELETON_OMITTED = " …"
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
    stderr.seek(0)
    report = stderr.read(DIAGNOSTIC_BYTES + 1)
    if len(report) > DIAGNOSTIC_BYTES:
        return "unknown"
    offset = 0
    for _ in range(MAX_REGISTRY_WARNINGS):
        warning = REGISTRY_WARNING.match(report, offset)
        if warning is None:
            break
        offset = warning.end()
    line, newline, trailing = report[offset:].partition(b"\n")
    if not newline:
        return "unknown"
    # After a warning, require exactly one remaining error line through EOF.
    # An embedded newline in registry-supplied warning text can mimic output.
    if offset and trailing:
        return "unknown"
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
    # Bearer-token retrieval can wrap a second request inside the first.
    token_request = re.match(rb'(?:Get|Post) "https://ghcr\.io/token(?:\?[^"\x00-\x20\x7f]*)?": ', detail)
    if token_request:
        detail = detail[token_request.end():]
    # A certificate the runner cannot verify is not transient: not retried.
    if detail.startswith(b"tls: failed to verify certificate: "):
        return "transport-tls"
    if (detail in (b"EOF", b"unexpected EOF", b"net/http: TLS handshake timeout")
            or detail.startswith((b"dial tcp: ", b"dial tcp "))):
        return "transport"
    return "unknown"


def path_kind(path):
    if not path or path == b"/v2/":
        return "registry-root"
    for marker, kind in ((b"/blobs/uploads", "blob-upload"), (b"/blobs/", "blob"), (b"/manifests/", "manifest"),
                         (b"/referrers/", "referrers"), (b"/token", "token")):
        if marker in path:
            return kind
    return "other"


def read_report(stderr):
    """The first DIAGNOSTIC_BYTES of stderr, cut back to whole lines, and whether it was cut."""
    stderr.seek(0)
    report = stderr.read(DIAGNOSTIC_BYTES + 1)
    if len(report) <= DIAGNOSTIC_BYTES:
        return report, False
    whole = report[:DIAGNOSTIC_BYTES]
    return whole[: whole.rfind(b"\n") + 1], True


def terminal(stderr):
    report, _ = read_report(stderr)
    return TERMINAL.search(report) is not None


def copy_signals(stderr):
    """What the copy's stderr says, in fixed words: status, OCI error code, request,
    registry host class, transport markers and size. Safe to print."""
    report, cut = read_report(stderr)
    # The status only from a request record (`"<url>": response status code NNN: `),
    # never from message text.
    status = STATUS.search(report)
    code = next((m[1] for m in (OCI_CODE.match(line) for line in report.split(b"\n")) if m), None)
    request = REQUEST.search(report)
    if request:
        host = "ghcr.io" if request[2].lower() == b"ghcr.io" else "other"
        described = f"{request[1].decode().upper()} {path_kind(request[3])}"
    else:
        host, described = "none", "none"
    markers = [name for name, needle in TRANSPORT_MARKERS if needle in report]
    lines = report.count(b"\n")
    return (f"status {status[1].decode() if status else 'none'}; "
            f"oci-code {code.decode().replace(' ', '-') if code else 'none'}; "
            f"request {described}; host {host}; transport {','.join(markers) or 'none'}; "
            f"stderr {lines} lines" + (" (first 4096 bytes)" if cut else ""))


def first_line(stderr):
    """The first line of stderr within DIAGNOSTIC_BYTES, whether it was cut
    there, and whether stderr is empty."""
    stderr.seek(0)
    head = stderr.read(DIAGNOSTIC_BYTES + 1)
    line = re.split(rb"[\r\n]", head[:DIAGNOSTIC_BYTES], maxsplit=1)
    cut = len(line) == 1 and len(head) > DIAGNOSTIC_BYTES
    # A cut token may read as a listed word: it is dropped.
    return (line[0][: line[0].rfind(b" ") + 1] if cut else line[0]), cut, not head


def skeleton(stderr):
    """The first stderr line as the name of its form, then only reviewed words,
    a status after "status code", ':' and ',' kept, everything else one
    placeholder per run, bounded (marker included). "unrecognised: " and the
    whole line masked for any other line, "blank" for an empty line and "none"
    only for an empty stderr. Safe to print."""
    line, cut, empty = first_line(stderr)
    if empty:
        return "none"
    if not line.strip():
        return "blank"
    for form, name in SKELETON_FORMS:
        prefix = form.match(line)
        if prefix:
            # The origin and type come from fixed alternations, never the input.
            name = name or f"error from {prefix[1].decode()} {prefix[2].decode()} for <x>:"
            rest = line[prefix.end():]
            break
    else:
        name, rest = "unrecognised:", line
    pieces = [name]
    for match in SKELETON_TOKEN.finditer(rest):
        token = match.group()
        if token in (b":", b","):
            piece = token.decode()
        elif SKELETON_WORD.fullmatch(token) and token.lower().decode() in SKELETON_WORDS:
            piece = token.lower().decode()
        elif SKELETON_STATUS.fullmatch(token) and pieces[-2:] == ["status", "code"]:
            piece = token.decode()
        else:
            piece = "<x>"
        if piece == "<x>" and pieces[-1] == "<x>":
            continue
        pieces.append(piece)
    text = ""
    for count, piece in enumerate(pieces):
        part = piece if not text or piece in (":", ",") else " " + piece
        if count == SKELETON_TOKENS or len(text) + len(part) > SKELETON_CHARS - len(SKELETON_OMITTED):
            return text + SKELETON_OMITTED
        text += part
    return text + SKELETON_OMITTED if cut else text


def attempt_copy(tool, args, env, work, image, timeout):
    """None on success, else (category, signals, retryable) of one copy attempt."""
    signals = "status none; oci-code none; request none; host none; transport none; stderr 0 lines"
    try:
        with tempfile.TemporaryFile(dir=work) as err:
            try:
                result = subprocess.run([str(tool), *args], input=b"", env=env, cwd=work,
                                        stdout=subprocess.DEVNULL, stderr=err, timeout=timeout, check=False)
            except subprocess.TimeoutExpired:
                return "deadline", f'{copy_signals(err)}; first-line "{skeleton(err)}"', False
            if result.returncode == 0:
                return None
            signals = f'{copy_signals(err)}; first-line "{skeleton(err)}"'
            if result.returncode < 0:
                return "signal", signals, False
            if result.returncode == 1:
                category = copy_failure_category(err, image)
                return category, signals, category in TRANSIENT and not terminal(err)
            return "unknown", signals, False
    except (OSError, subprocess.SubprocessError):
        return "execution", signals, False


def wait(seconds):
    time.sleep(seconds)


def copy(tool, args, env, work, image):
    """Copy, retrying only a transient failure, within COPY_BUDGET seconds in all."""
    deadline = time.monotonic() + COPY_BUDGET
    for attempt in range(1, COPY_ATTEMPTS + 1):
        outcome = attempt_copy(tool, args, env, work, image, deadline - time.monotonic())
        if outcome is None:
            return
        category, signals, retryable = outcome
        pause = COPY_BACKOFF[attempt - 1] if attempt < COPY_ATTEMPTS else 0
        if retryable and attempt < COPY_ATTEMPTS and deadline - time.monotonic() >= pause + MIN_ATTEMPT:
            print(f"oci-stage: copy attempt {attempt} of {COPY_ATTEMPTS} failed ({category}) "
                  f"[{signals}]; retrying in {pause} s", file=sys.stderr)
            wait(pause)
            # The pause may have run long: an attempt starts only with a minute left.
            if deadline - time.monotonic() < MIN_ATTEMPT:
                raise Refusal(f"copy failed ({category}) [attempt {attempt} of {COPY_ATTEMPTS}; {signals}]")
            continue
        raise Refusal(f"copy failed ({category}) [attempt {attempt} of {COPY_ATTEMPTS}; {signals}]")


def run(tool, args, env, work, reason, password=b""):
    try:
        # Discard stdout and keep stderr in a private temporary file so even
        # large child output never has to be buffered in memory.
        with tempfile.TemporaryFile(dir=work) as err:
            result = subprocess.run([str(tool), *args], input=password, env=env,
                                    cwd=work, stdout=subprocess.DEVNULL, stderr=err, timeout=LOGIN_TIMEOUT,
                                    check=False)
            if result.returncode == 0:
                return
    except (subprocess.TimeoutExpired, OSError, subprocess.SubprocessError):
        pass
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
        copy(tool, ["cp", "--force", "--no-tty", "--from-oci-layout",
                    "--to-registry-config", str(auth), "--to-distribution-spec",
                    "v1.1-referrers-api", str(archive) + "@" + args.root, args.image],
             env, work, args.image)


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
