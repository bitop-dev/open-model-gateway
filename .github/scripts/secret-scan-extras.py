#!/usr/bin/env python3
"""Write what `gitleaks git` does not read, as plain files for `gitleaks dir` (#603).

Usage: secret-scan-extras.py OUT [--tag-messages] REV...

REV... are `git log` revision arguments (a range `base..head`, one commit, or
`refs/remotes/origin/main --tags`). Into OUT, which must not exist, it writes:

- messages.txt: the message of every commit in the revisions, merges included;
- tags.txt (with --tag-messages): the message of every annotated tag;
- names.txt: every path a commit in the revisions adds, changes or renames;
- archives/: every archive blob those commits add, under its own extension so
  that `gitleaks dir --max-archive-depth` opens it (`.tgz`, which gitleaks does
  not recognise, is written as `.tar.gz`);
- utf16/: every UTF-16 text blob they add (a byte order mark, or a NUL in every
  other byte of its start), decoded to UTF-8, since gitleaks reads bytes as UTF-8.

It only reads git objects and writes files: nothing from the repository is run.
It fails (exit 2) on a git error and (exit 3) when a blob or the total would
pass the bounds below, so a scan never quietly covers less than it says.
"""

import os
import subprocess
import sys
import threading

ARCHIVE_SUFFIXES = (".zip", ".jar", ".tar", ".tar.gz", ".tgz", ".gz")
def bound(name, default):
    # A test may lower a bound (SECRET_SCAN_EXTRAS_MAX_BLOB, ..._MAX_TOTAL), never raise it.
    value = os.environ.get(f"SECRET_SCAN_EXTRAS_{name}", "")
    return min(default, int(value)) if value.isdigit() else default


MAX_BLOB = bound("MAX_BLOB", 64 * 1024 * 1024)
MAX_TOTAL = bound("MAX_TOTAL", 512 * 1024 * 1024)
UTF16_SNIFF = 512


def fail(code, message):
    sys.stderr.write(f"secret-scan-extras: {message}\n")
    sys.exit(code)


def git(*args, data=None):
    done = subprocess.run(["git", *args], input=data, capture_output=True, check=False)
    if done.returncode != 0:
        fail(2, f"git {args[0]} failed: {done.stderr.decode('utf-8', 'replace').strip()[:200]}")
    return done.stdout


def archive_name(path):
    lower = path.lower()
    for suffix in ARCHIVE_SUFFIXES:
        if lower.endswith(suffix):
            return ".tar.gz" if suffix == ".tgz" else suffix
    return None


def utf16(head):
    if head[:2] in (b"\xff\xfe", b"\xfe\xff"):
        return True
    sample = head[:UTF16_SNIFF]
    if len(sample) < 8:
        return False
    even = sample[0::2]
    odd = sample[1::2]
    # ASCII text in UTF-16 has a NUL in every other byte (odd bytes for
    # little-endian, even bytes for big-endian).
    return odd.count(0) >= 0.9 * len(odd) or even.count(0) >= 0.9 * len(even)


def decode16(blob):
    if blob[:2] == b"\xfe\xff":
        return blob[2:].decode("utf-16-be", "replace")
    if blob[:2] == b"\xff\xfe":
        return blob[2:].decode("utf-16-le", "replace")
    sample = blob[:UTF16_SNIFF]
    big = sample[0::2].count(0) > sample[1::2].count(0)
    return blob.decode("utf-16-be" if big else "utf-16-le", "replace")


def added_blobs(revs):
    """(blob id, path) of every blob the commits in `revs` add, change or rename to."""
    out = []
    raw = git("log", "-m", "--format=", "--raw", "--no-abbrev", "--no-renames", *revs)
    for line in raw.decode("utf-8", "surrogateescape").splitlines():
        if not line.startswith(":"):
            continue
        meta, _, path = line.partition("\t")
        fields = meta.split()
        if len(fields) < 5 or fields[4] == "D" or fields[1] == "160000":
            continue
        out.append((fields[3], path))
    return out


def main(argv):
    if len(argv) < 2:
        fail(2, "usage: secret-scan-extras.py OUT [--tag-messages] REV...")
    out = argv[0]
    rest = argv[1:]
    tag_messages = rest[:1] == ["--tag-messages"]
    revs = rest[1:] if tag_messages else rest
    if not revs:
        fail(2, "no revisions given")
    os.makedirs(out)
    for sub in ("archives", "utf16"):
        os.makedirs(os.path.join(out, sub))

    messages = git("log", "-m", "--format=commit %H%n%B", *revs)
    with open(os.path.join(out, "messages.txt"), "wb") as f:
        f.write(messages)
    if tag_messages:
        tags = git("for-each-ref", "refs/tags", "--format=tag %(refname:short)%0a%(contents)")
        with open(os.path.join(out, "tags.txt"), "wb") as f:
            f.write(tags)

    blobs = added_blobs(revs)
    names = sorted({path for _, path in blobs})
    with open(os.path.join(out, "names.txt"), "w", encoding="utf-8", errors="surrogateescape") as f:
        f.write("".join(f"{name}\n" for name in names))

    unique = {}
    for blob, path in blobs:
        unique.setdefault(blob, path)
    total = 0
    archives = texts = 0
    ordered = sorted(unique.items(), key=lambda item: item[1])
    # One `git cat-file --batch` for every blob, read in order: a process per blob
    # would take minutes over the whole history.
    # The ids go in from a thread: written all at once before reading, they would
    # block on a full output pipe.
    batch = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)

    def feed():
        with batch.stdin:
            batch.stdin.write("".join(f"{blob}\n" for blob, _ in ordered).encode())

    feeder = threading.Thread(target=feed, daemon=True)
    feeder.start()
    for index, (blob, path) in enumerate(ordered):
        header = batch.stdout.readline().split()
        if len(header) != 3:
            fail(2, f"git cat-file did not return {blob}")
        size = int(header[2])
        data = batch.stdout.read(size)
        batch.stdout.read(1)
        if header[1] != b"blob":
            continue
        suffix = archive_name(path)
        if suffix is None and not utf16(data[:UTF16_SNIFF]):
            continue
        if size > MAX_BLOB:
            fail(3, f"{path} is {size} bytes, over the {MAX_BLOB}-byte bound for an archive or UTF-16 blob")
        total += size
        if total > MAX_TOTAL:
            fail(3, f"archives and UTF-16 blobs pass the {MAX_TOTAL}-byte bound")
        base = os.path.basename(path) or "blob"
        if suffix is not None:
            lower = base.lower()
            cut = 4 if lower.endswith(".tgz") else len(suffix)
            stem = base[:-cut] if len(base) > cut else "archive"
            with open(os.path.join(out, "archives", f"{index:06d}-{stem}{suffix}"), "wb") as f:
                f.write(data)
            archives += 1
        else:
            with open(os.path.join(out, "utf16", f"{index:06d}-{base}.txt"), "w", encoding="utf-8", errors="replace") as f:
                f.write(decode16(data))
            texts += 1
    feeder.join()
    if batch.wait() != 0:
        fail(2, "git cat-file failed")
    commits = git("rev-list", "--count", *revs).decode().strip()
    sys.stderr.write(
        f"secret-scan-extras: {commits} commit messages, {len(names)} paths, "
        f"{archives} archives, {texts} UTF-16 files\n"
    )


if __name__ == "__main__":
    main(sys.argv[1:])
